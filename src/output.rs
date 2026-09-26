use std::io::{self, BufReader, Read};

use unicode_width::UnicodeWidthChar;

use crate::ansi::{Attr, apply_sgr};
use crate::window::Window;

const TAB_WIDTH: usize = 8;
const MAX_ANSIBUF: usize = 100;
/// glibc's `MB_CUR_MAX` for UTF-8 locales. Upstream gives up on a character
/// (and treats it as end of output) after this many bytes fail to decode.
const UTF8_MB_CUR_MAX: usize = 6;

#[derive(Clone, Copy, Debug, Default)]
pub struct Flags {
    pub color: bool,
    pub differences: bool,
    pub cumulative: bool,
    /// Any of `--differences`, `--chgexit` or `--equexit`.
    pub track_changes: bool,
    pub follow: bool,
    pub no_wrap: bool,
    pub utf8: bool,
}

/// What `display_char` remembers about the cell(s) it last compared against.
///
/// It persists across runs, like the function-static variables it mirrors,
/// so that a combining mark following its base character is compared as part
/// of that same cell.
#[derive(Clone, Debug)]
pub struct DiffCache {
    y: isize,
    x: isize,
    /// Per column, the characters of the old cell that are not matched yet.
    columns: Vec<Vec<Option<char>>>,
    standout: bool,
}

impl Default for DiffCache {
    fn default() -> Self {
        DiffCache {
            y: -1,
            x: -1,
            columns: Vec::new(),
            standout: false,
        }
    }
}

struct Input<R: Read> {
    inner: BufReader<R>,
    pushback: Option<u8>,
    utf8: bool,
}

impl<R: Read> Input<R> {
    fn getc(&mut self) -> Option<u8> {
        if let Some(b) = self.pushback.take() {
            return Some(b);
        }
        let mut buf = [0u8; 1];
        loop {
            return match self.inner.read(&mut buf) {
                Ok(1) => Some(buf[0]),
                Ok(_) => None,
                Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                Err(_) => None,
            };
        }
    }

    fn ungetc(&mut self, b: u8) {
        self.pushback = Some(b);
    }

    /// Reads one character like procps' `getmb`, including how it fails on
    /// bad input: invalid lead bytes are skipped, but once a multi-byte
    /// sequence has started, an invalid byte leaves glibc's conversion state
    /// stuck, and upstream then reports end of output.
    fn getmb(&mut self) -> Option<char> {
        if !self.utf8 {
            return self.getc().filter(u8::is_ascii).map(char::from);
        }
        let mut consumed = 0;
        let mut need = 0;
        let mut cp: u32 = 0;
        let mut min = 0;
        loop {
            let b = self.getc()?;
            if need == 0 {
                match b {
                    0x00..=0x7f => return Some(char::from(b)),
                    0xc2..=0xdf => (need, cp, min) = (1, u32::from(b & 0x1f), 0x80),
                    0xe0..=0xef => (need, cp, min) = (2, u32::from(b & 0x0f), 0x800),
                    0xf0..=0xf4 => (need, cp, min) = (3, u32::from(b & 0x07), 0x10000),
                    _ => {}
                }
            } else if b & 0xc0 == 0x80 {
                cp = (cp << 6) | u32::from(b & 0x3f);
                need -= 1;
                if need == 0 {
                    return char::from_u32(cp).filter(|_| cp >= min);
                }
            } else {
                return None;
            }
            consumed += 1;
            if consumed == UTF8_MB_CUR_MAX {
                return None;
            }
        }
    }

    fn skip_to_eol(&mut self) {
        while let Some(c) = self.getmb() {
            if c == '\n' {
                break;
            }
        }
    }

    fn drain(&mut self) {
        self.pushback = None;
        let _ = io::copy(&mut self.inner, &mut io::sink());
    }
}

/// glibc's `iswprint` and `wcwidth` for the characters that reach it:
/// `None` for characters that are not printable.
fn printable_width(c: char) -> Option<usize> {
    if c.is_control() {
        None
    } else {
        Some(UnicodeWidthChar::width(c).unwrap_or(0))
    }
}

struct Feeder<'a, R: Read> {
    input: Input<R>,
    win: &'a mut Window,
    cache: &'a mut DiffCache,
    flags: Flags,
    first_screen: bool,
    sgr: Attr,
    bell: &'a mut dyn FnMut(),
}

/// Writes one run's command output into `win` following upstream's
/// `run_command`, then reads and discards whatever output does not fit.
/// Returns whether the visible contents changed; that is only meaningful
/// when `flags.track_changes` is set and this is not the first screen.
pub fn render_output<R: Read>(
    input: R,
    win: &mut Window,
    cache: &mut DiffCache,
    flags: Flags,
    first_screen: bool,
    bell: &mut dyn FnMut(),
) -> bool {
    let mut feeder = Feeder {
        input: Input {
            inner: BufReader::new(input),
            pushback: None,
            utf8: flags.utf8,
        },
        win,
        cache,
        flags,
        first_screen,
        sgr: Attr::DEFAULT,
        bell,
    };
    feeder.reset_ansi();
    let changed = feeder.run();
    feeder.input.drain();
    changed
}

impl<R: Read> Feeder<'_, R> {
    fn reset_ansi(&mut self) {
        self.sgr = Attr::DEFAULT;
        if self.flags.color {
            self.win.attr = self.sgr;
        }
    }

    fn run(&mut self) -> bool {
        let width = self.win.width();
        let height = self.win.height();
        let follow = self.flags.follow;
        let mut changed = false;
        let mut carry: Option<char> = None;
        let mut y = 0usize;

        while y < height || follow {
            let mut x = 0usize;
            let mut eof = false;
            loop {
                let Some(c) = carry.take().or_else(|| self.input.getmb()) else {
                    if !follow {
                        changed |= self.clrtobot(y, x);
                    }
                    eof = true;
                    break;
                };
                if c == '\n' {
                    if follow {
                        self.win.add_newline();
                    } else {
                        changed |= self.clrtoeol(y, x);
                    }
                    break;
                }
                if c == '\x1b' {
                    self.process_ansi();
                    continue;
                }
                if c == '\x07' {
                    (self.bell)();
                    continue;
                }
                let cwid = if c == '\t' {
                    // One free column is enough to consider a tab printed.
                    1
                } else {
                    match printable_width(c) {
                        Some(w) => w,
                        None => continue,
                    }
                };

                if cwid > width - x {
                    if self.flags.no_wrap {
                        self.input.skip_to_eol();
                        self.reset_ansi();
                    } else {
                        carry = Some(c);
                    }
                    changed |= self.clrtoeol(y, x);
                    break;
                }

                if c == '\t' {
                    loop {
                        changed |= self.display_char(y, x as isize, ' ', 1);
                        x += 1;
                        if x.is_multiple_of(TAB_WIDTH) || x >= width {
                            break;
                        }
                    }
                } else {
                    // A zero-width character modifies the preceding one.
                    let target = x as isize - isize::from(cwid == 0);
                    changed |= self.display_char(y, target, c, cwid);
                    x += cwid;
                }
            }
            if eof {
                break;
            }
            y += 1;
        }
        changed
    }

    /// Handles an escape sequence after its ESC was read. Only SGR sequences
    /// are understood; anything else is consumed up to its first character
    /// that is neither a digit nor a semicolon.
    fn process_ansi(&mut self) {
        if !self.flags.color {
            return;
        }
        let mut c = self.input.getc();
        if c == Some(b'(') {
            self.input.getc();
            c = self.input.getc();
        }
        if c != Some(b'[') {
            if let Some(b) = c {
                self.input.ungetc(b);
            }
            return;
        }
        let mut params = String::new();
        for _ in 0..MAX_ANSIBUF {
            match self.input.getc() {
                Some(b'm') => {
                    apply_sgr(&mut self.sgr, &mut self.win.attr, &params);
                    return;
                }
                Some(b) if b.is_ascii_digit() || b == b';' => params.push(char::from(b)),
                _ => return,
            }
        }
    }

    fn clrtoeol(&mut self, y: usize, x: usize) -> bool {
        if self.flags.track_changes {
            let mut changed = false;
            for x in x..self.win.width() {
                changed |= self.display_char(y, x as isize, ' ', 1);
            }
            return changed;
        }
        self.win.mv(y as isize, x as isize);
        self.win.clrtoeol();
        false
    }

    fn clrtobot(&mut self, y: usize, x: usize) -> bool {
        if self.flags.track_changes {
            let mut changed = false;
            let mut x = x;
            for y in y..self.win.height() {
                for x in x..self.win.width() {
                    changed |= self.display_char(y, x as isize, ' ', 1);
                }
                x = 0;
            }
            return changed;
        }
        if !self.flags.follow {
            self.win.mv(y as isize, x as isize);
            self.win.clrtobot();
        }
        false
    }

    /// Writes `c` at (`y`, `x`) and reports whether that changed the screen.
    /// The comparison is against whatever the cells held before, so it also
    /// sees changes in parts of a double-width or combined character.
    fn display_char(&mut self, y: usize, x: isize, c: char, cwid: usize) -> bool {
        let mut changed = false;
        let mut old_standout = false;

        if !self.first_screen && self.flags.track_changes {
            let cache = &mut *self.cache;
            if cache.y != y as isize || cache.x != x {
                cache.y = y as isize;
                cache.x = x;
                cache.standout = false;
                cache.columns.clear();
                for i in 0..cwid {
                    let cell = self.win.cell(y, x as usize + i);
                    cache.standout |= cell.standout;
                    cache.columns.push(cell.chars().map(Some).collect());
                }
            }

            for column in &mut cache.columns {
                match column.iter_mut().find(|slot| **slot == Some(c)) {
                    Some(slot) => *slot = None,
                    None => {
                        cache.columns.clear();
                        break;
                    }
                }
            }
            changed = cache.columns.is_empty()
                || cache
                    .columns
                    .iter()
                    .any(|column| column.iter().any(Option::is_some));
            old_standout = cache.standout;
        }

        if !self.flags.follow {
            self.win.mv(y as isize, x);
        }
        if cwid > 0 {
            self.win.add_char(c, cwid);
        } else {
            self.win.add_mark(c);
        }

        if self.flags.differences {
            let standout = changed || (self.flags.cumulative && old_standout);
            self.win.chgat(y as isize, x, self.win.attr, standout);
        }
        changed
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ansi::Color;

    fn flags() -> Flags {
        Flags {
            utf8: true,
            ..Flags::default()
        }
    }

    fn feed(
        win: &mut Window,
        cache: &mut DiffCache,
        flags: Flags,
        first: bool,
        out: &[u8],
    ) -> bool {
        render_output(out, win, cache, flags, first, &mut || {})
    }

    fn render(h: usize, w: usize, flags: Flags, out: &[u8]) -> Window {
        let mut win = Window::new(h, w);
        feed(&mut win, &mut DiffCache::default(), flags, true, out);
        win
    }

    fn rows(win: &Window) -> Vec<String> {
        (0..win.height())
            .map(|y| win.row_text(y).trim_end().to_string())
            .collect()
    }

    #[test]
    fn long_lines_wrap_and_output_is_cut_at_the_bottom() {
        let win = render(2, 4, flags(), b"abcdefghij\nzz\n");
        assert_eq!(rows(&win), vec!["abcd", "efgh"]);
    }

    #[test]
    fn line_filling_the_width_exactly_does_not_add_a_blank_line() {
        let win = render(3, 4, flags(), b"abcd\nef\n");
        assert_eq!(rows(&win), vec!["abcd", "ef", ""]);
    }

    #[test]
    fn no_wrap_truncates_and_resets_color() {
        let f = Flags {
            no_wrap: true,
            color: true,
            ..flags()
        };
        let win = render(2, 4, f, b"\x1b[31mabcdef\nxy\n");
        assert_eq!(rows(&win), vec!["abcd", "xy"]);
        assert_eq!(win.cell(0, 0).attr.fg, Color::Indexed(1));
        assert_eq!(win.cell(1, 0).attr.fg, Color::Default);
    }

    #[test]
    fn tabs_expand_to_the_next_stop_and_wrap_like_characters() {
        let win = render(3, 12, flags(), b"a\tb\tc\n");
        assert_eq!(rows(&win), vec!["a       b", "c", ""]);
    }

    #[test]
    fn escapes_without_color_drop_only_the_escape_byte() {
        let win = render(1, 20, flags(), b"\x1b[31mred\x1b[0m\n");
        assert_eq!(rows(&win), vec!["[31mred[0m"]);
    }

    #[test]
    fn charset_designation_is_skipped_in_color_mode() {
        let f = Flags {
            color: true,
            ..flags()
        };
        let win = render(1, 20, f, b"\x1b(B\x1b[mplain\x1b[?25lx\n");
        assert_eq!(rows(&win), vec!["plain25lx"]);
    }

    #[test]
    fn color_persists_across_lines_until_reset() {
        let f = Flags {
            color: true,
            ..flags()
        };
        let win = render(2, 10, f, b"\x1b[31mred\nstill\n");
        assert_eq!(win.cell(1, 0).attr.fg, Color::Indexed(1));
        assert_eq!(win.cell(0, 5).attr.fg, Color::Default);
    }

    #[test]
    fn wide_characters_wrap_when_they_do_not_fit() {
        let win = render(2, 3, flags(), "ab日\n".as_bytes());
        assert_eq!(rows(&win), vec!["ab", "日"]);
    }

    #[test]
    fn combining_marks_join_the_previous_cell() {
        let win = render(1, 5, flags(), "e\u{301}z\n".as_bytes());
        assert_eq!(win.row_text(0), "e\u{301}z   ");
    }

    #[test]
    fn invalid_lead_bytes_are_dropped() {
        let win = render(1, 5, flags(), b"a\xffb\n");
        assert_eq!(rows(&win), vec!["ab"]);
    }

    #[test]
    fn broken_multibyte_sequence_ends_output() {
        let win = render(2, 5, flags(), b"ab\n\xe2Ax\nmore\n");
        assert_eq!(rows(&win), vec!["ab", ""]);
    }

    #[test]
    fn non_utf8_locale_ends_output_at_first_non_ascii_byte() {
        let f = Flags {
            utf8: false,
            ..flags()
        };
        let win = render(2, 5, f, "a\u{e9}b\nnext\n".as_bytes());
        assert_eq!(rows(&win), vec!["a", ""]);
    }

    #[test]
    fn follow_mode_appends_and_scrolls() {
        let f = Flags {
            follow: true,
            ..flags()
        };
        let mut win = Window::new(3, 4);
        win.scroll = true;
        let mut cache = DiffCache::default();
        feed(&mut win, &mut cache, f, true, b"abcd\nx\n");
        assert_eq!(rows(&win), vec!["", "x", ""]);
        feed(&mut win, &mut cache, f, false, b"y\n");
        assert_eq!(rows(&win), vec!["x", "y", ""]);
    }

    #[test]
    fn differences_highlight_changed_cells() {
        let f = Flags {
            differences: true,
            track_changes: true,
            ..flags()
        };
        let mut win = Window::new(1, 4);
        let mut cache = DiffCache::default();
        assert!(!feed(&mut win, &mut cache, f, true, b"abc\n"));
        assert!(feed(&mut win, &mut cache, f, false, b"abd\n"));
        let marked: Vec<bool> = (0..4).map(|x| win.cell(0, x).standout).collect();
        assert_eq!(marked, vec![false, false, true, false]);
        assert!(!feed(&mut win, &mut cache, f, false, b"abd\n"));
        assert!(!win.cell(0, 2).standout);
    }

    #[test]
    fn permanent_differences_keep_highlight() {
        let f = Flags {
            differences: true,
            cumulative: true,
            track_changes: true,
            ..flags()
        };
        let mut win = Window::new(1, 4);
        let mut cache = DiffCache::default();
        feed(&mut win, &mut cache, f, true, b"abc\n");
        feed(&mut win, &mut cache, f, false, b"aXc\n");
        assert!(!feed(&mut win, &mut cache, f, false, b"aXc\n"));
        assert!(win.cell(0, 1).standout);
    }

    #[test]
    fn full_screen_diff_leaves_cursor_on_last_row() {
        let f = Flags {
            differences: true,
            track_changes: true,
            ..flags()
        };
        let mut win = Window::new(3, 4);
        feed(
            &mut win,
            &mut DiffCache::default(),
            f,
            true,
            b"1\n2\n3\n4\n",
        );
        assert_eq!(win.cursor().0, 2);
    }

    #[test]
    fn unterminated_last_line_leaves_cursor_on_it() {
        let win = render(4, 4, flags(), b"one\ntwo");
        assert_eq!(win.cursor(), (1, 3));
    }
}
