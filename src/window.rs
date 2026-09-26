use unicode_width::UnicodeWidthChar;

use crate::ansi::Attr;

/// Upper bound on the characters one cell holds (a spacing character plus
/// combining marks), matching ncurses' `CCHARW_MAX - 1`.
const MAX_CELL_CHARS: usize = 4;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Cell {
    pub ch: char,
    pub marks: Vec<char>,
    pub attr: Attr,
    pub standout: bool,
    /// Set on the right half of a double-width character; `ch` and `marks`
    /// repeat those of the left half.
    pub cont: bool,
}

impl Cell {
    pub fn blank() -> Self {
        Cell {
            ch: ' ',
            marks: Vec::new(),
            attr: Attr::DEFAULT,
            standout: false,
            cont: false,
        }
    }

    /// All characters stored in the cell: the spacing character first.
    pub fn chars(&self) -> impl Iterator<Item = char> + '_ {
        std::iter::once(self.ch).chain(self.marks.iter().copied())
    }

    fn display_width(&self) -> usize {
        UnicodeWidthChar::width(self.ch).unwrap_or(1).clamp(1, 2)
    }
}

/// A character grid with an ncurses-style cursor.
///
/// Only the behaviour watch depends on is modelled, but that is modelled
/// exactly: several user-visible details of the C implementation (which rows
/// a screenshot saves, the blank line after a full-width line in follow
/// mode, where a failed move leaves the cursor) fall out of ncurses' cursor
/// rules rather than any explicit logic.
#[derive(Clone, Debug)]
pub struct Window {
    height: usize,
    width: usize,
    rows: Vec<Vec<Cell>>,
    cy: usize,
    cx: usize,
    /// ncurses' `_WRAPPED`: the cursor wrapped since it was last placed.
    wrapped: bool,
    pub scroll: bool,
    /// Rendition applied to characters added from now on.
    pub attr: Attr,
}

impl Window {
    pub fn new(height: usize, width: usize) -> Self {
        let height = height.max(1);
        let width = width.max(1);
        Window {
            height,
            width,
            rows: vec![vec![Cell::blank(); width]; height],
            cy: 0,
            cx: 0,
            wrapped: false,
            scroll: false,
            attr: Attr::DEFAULT,
        }
    }

    pub fn height(&self) -> usize {
        self.height
    }

    pub fn width(&self) -> usize {
        self.width
    }

    pub fn cursor(&self) -> (usize, usize) {
        (self.cy, self.cx)
    }

    pub fn cell(&self, y: usize, x: usize) -> &Cell {
        &self.rows[y][x]
    }

    /// Changes the size, keeping the overlapping contents like `wresize`.
    pub fn resize(&mut self, height: usize, width: usize) {
        let height = height.max(1);
        let width = width.max(1);
        for row in &mut self.rows {
            row.resize(width, Cell::blank());
            if let Some(last) = row.last_mut()
                && !last.cont
                && last.display_width() == 2
            {
                *last = Cell::blank();
            }
        }
        self.rows.resize(height, vec![Cell::blank(); width]);
        self.height = height;
        self.width = width;
        self.cy = self.cy.min(height - 1);
        self.cx = self.cx.min(width - 1);
    }

    /// `wmove`: fails, leaving the cursor where it was, outside the window.
    pub fn mv(&mut self, y: isize, x: isize) -> bool {
        if y < 0 || x < 0 || y as usize >= self.height || x as usize >= self.width {
            return false;
        }
        self.cy = y as usize;
        self.cx = x as usize;
        self.wrapped = false;
        true
    }

    /// Blanks whatever double-width characters would be split by writing
    /// columns `x0..=x1` of row `y`.
    fn clear_overlaps(&mut self, y: usize, x0: usize, x1: usize) {
        if self.rows[y][x0].cont && x0 > 0 {
            self.rows[y][x0 - 1] = Cell::blank();
        }
        if x1 + 1 < self.width && self.rows[y][x1 + 1].cont {
            self.rows[y][x1 + 1] = Cell::blank();
        }
    }

    /// Moves the cursor to the start of the next line after the last column
    /// was written. Fails at the bottom unless scrolling is enabled, leaving
    /// the cursor on the last column.
    fn wrap(&mut self) -> bool {
        self.wrapped = true;
        if self.cy + 1 >= self.height {
            self.cx = self.width - 1;
            if !self.scroll {
                return false;
            }
            self.scroll_up();
        } else {
            self.cy += 1;
        }
        self.cx = 0;
        true
    }

    fn scroll_up(&mut self) {
        self.rows.remove(0);
        self.rows.push(vec![Cell::blank(); self.width]);
    }

    fn put(&mut self, mut cell: Cell, cwid: usize) -> bool {
        if cwid > self.width - self.cx {
            for x in self.cx..self.width {
                self.rows[self.cy][x] = Cell::blank();
            }
            if !self.wrap() || cwid > self.width {
                return false;
            }
        }
        let (y, x) = (self.cy, self.cx);
        self.clear_overlaps(y, x, x + cwid - 1);
        cell.cont = false;
        if cwid == 2 {
            let mut right = cell.clone();
            right.cont = true;
            self.rows[y][x + 1] = right;
        }
        self.rows[y][x] = cell;
        self.cx += cwid;
        if self.cx >= self.width {
            return self.wrap();
        }
        true
    }

    /// `waddnwstr` of one spacing character of width `cwid` (1 or 2).
    pub fn add_char(&mut self, ch: char, cwid: usize) -> bool {
        let cell = Cell {
            ch,
            marks: Vec::new(),
            attr: self.attr,
            standout: false,
            cont: false,
        };
        self.put(cell, cwid)
    }

    /// Adds a combining mark to the character under the cursor and rewrites
    /// it there, as upstream does with `win_wch` followed by `wadd_wch`.
    pub fn add_mark(&mut self, mark: char) -> bool {
        let mut cell = self.rows[self.cy][self.cx].clone();
        if 1 + cell.marks.len() < MAX_CELL_CHARS {
            cell.marks.push(mark);
        }
        let cwid = cell.display_width();
        self.put(cell, cwid)
    }

    /// `waddch(win, '\n')`.
    pub fn add_newline(&mut self) -> bool {
        self.clrtoeol();
        if self.cy + 1 >= self.height {
            if !self.scroll {
                return false;
            }
            self.scroll_up();
        } else {
            self.cy += 1;
        }
        self.cx = 0;
        self.wrapped = false;
        true
    }

    /// `waddstr`: characters that are not printable are skipped and combining
    /// marks join the preceding character.
    pub fn add_str(&mut self, s: &str) {
        for ch in s.chars() {
            let ok = match UnicodeWidthChar::width(ch) {
                Some(0) if self.cx > 0 => {
                    self.cx -= 1;
                    if self.rows[self.cy][self.cx].cont && self.cx > 0 {
                        self.cx -= 1;
                    }
                    self.add_mark(ch)
                }
                Some(0) => true,
                Some(w) => self.add_char(ch, w),
                None => true,
            };
            if !ok {
                return;
            }
        }
    }

    /// Right after a wrap this clears the new line, except in the bottom-right
    /// corner, where it does nothing.
    pub fn clrtoeol(&mut self) {
        if self.wrapped && self.cy + 1 < self.height {
            self.wrapped = false;
        }
        if self.wrapped {
            return;
        }
        let (y, x) = (self.cy, self.cx);
        self.clear_overlaps(y, x, x);
        for cell in &mut self.rows[y][x..] {
            *cell = Cell::blank();
        }
    }

    pub fn clrtobot(&mut self) {
        let (y, x) = (self.cy, self.cx);
        self.clear_overlaps(y, x, x);
        for cell in &mut self.rows[y][x..] {
            *cell = Cell::blank();
        }
        for row in &mut self.rows[self.cy + 1..] {
            row.fill(Cell::blank());
        }
    }

    /// `mvwchgat(win, y, x, 1, ...)`: restyles one cell without moving
    /// characters, leaving the cursor at the cell.
    pub fn chgat(&mut self, y: isize, x: isize, attr: Attr, standout: bool) {
        if self.mv(y, x) {
            let cell = &mut self.rows[self.cy][self.cx];
            cell.attr = attr;
            cell.standout = standout;
        }
    }

    /// The row's text as `winnstr` returns it: every column, including
    /// trailing blanks, with each double-width character once.
    pub fn row_text(&self, y: usize) -> String {
        let mut s = String::new();
        for cell in self.rows[y].iter().filter(|c| !c.cont) {
            s.extend(cell.chars());
        }
        s
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn text(w: &Window) -> Vec<String> {
        (0..w.height()).map(|y| w.row_text(y)).collect()
    }

    #[test]
    fn writing_the_last_column_wraps_the_cursor() {
        let mut w = Window::new(2, 3);
        for c in "abc".chars() {
            assert!(w.add_char(c, 1));
        }
        assert_eq!(w.cursor(), (1, 0));
    }

    #[test]
    fn bottom_right_without_scrolling_keeps_the_cursor_in_place() {
        let mut w = Window::new(1, 2);
        assert!(w.add_char('a', 1));
        assert!(!w.add_char('b', 1));
        assert_eq!(w.cursor(), (0, 1));
        assert_eq!(text(&w), vec!["ab"]);
    }

    #[test]
    fn scrolling_window_scrolls_on_newline() {
        let mut w = Window::new(2, 3);
        w.scroll = true;
        w.add_str("a");
        w.add_newline();
        w.add_str("b");
        w.add_newline();
        w.add_str("c");
        assert_eq!(text(&w), vec!["b  ", "c  "]);
    }

    #[test]
    fn wide_characters_occupy_two_cells_and_are_reported_once() {
        let mut w = Window::new(1, 4);
        w.add_char('日', 2);
        w.add_char('x', 1);
        assert!(w.cell(0, 1).cont);
        assert_eq!(w.row_text(0), "日x ");
    }

    #[test]
    fn overwriting_half_of_a_wide_character_blanks_the_other_half() {
        let mut w = Window::new(1, 4);
        w.add_char('日', 2);
        w.mv(0, 1);
        w.add_char('x', 1);
        assert_eq!(w.row_text(0), " x  ");
    }

    #[test]
    fn marks_join_the_character_under_the_cursor() {
        let mut w = Window::new(1, 4);
        w.add_char('e', 1);
        w.mv(0, 0);
        w.add_mark('\u{301}');
        assert_eq!(w.row_text(0), "e\u{301}   ");
        assert_eq!(w.cursor(), (0, 1));
    }

    #[test]
    fn failed_move_leaves_cursor() {
        let mut w = Window::new(2, 2);
        w.mv(1, 1);
        assert!(!w.mv(0, 2));
        assert_eq!(w.cursor(), (1, 1));
    }

    #[test]
    fn resize_keeps_overlapping_contents() {
        let mut w = Window::new(2, 3);
        w.add_str("abcdef");
        w.resize(3, 2);
        assert_eq!(text(&w), vec!["ab", "de", "  "]);
    }
}
