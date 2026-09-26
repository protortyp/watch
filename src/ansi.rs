use std::fmt::Write as _;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Color {
    #[default]
    Default,
    /// An entry of the 256-color palette; 0-7 are the standard colors and
    /// 8-15 their bright variants.
    Indexed(u8),
    Rgb(u8, u8, u8),
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Attr {
    pub bold: bool,
    pub dim: bool,
    pub italic: bool,
    pub underline: bool,
    pub blink: bool,
    pub reverse: bool,
    pub fg: Color,
    pub bg: Color,
}

impl Attr {
    pub const DEFAULT: Attr = Attr {
        bold: false,
        dim: false,
        italic: false,
        underline: false,
        blink: false,
        reverse: false,
        fg: Color::Default,
        bg: Color::Default,
    };
}

/// Emulates C `strtol` on a run of ASCII digits followed by the `int`
/// conversion upstream applies: an empty field is 0 and values that overflow
/// `long` saturate before being truncated.
fn sgr_number(field: &str) -> i32 {
    let mut value: i64 = 0;
    for b in field.bytes() {
        value = value.saturating_mul(10).saturating_add(i64::from(b - b'0'));
    }
    value as i32
}

enum Extended {
    Color(Color, usize),
    NotUnderstood,
}

/// Parses the fields following a 38 or 48 code.
fn extended_color(rest: &[&str]) -> Extended {
    match rest.first() {
        Some(&"5") => match rest.get(1) {
            Some(n) => match sgr_number(n) {
                n @ 0..=255 => Extended::Color(Color::Indexed(n as u8), 2),
                _ => Extended::NotUnderstood,
            },
            None => Extended::NotUnderstood,
        },
        // 24-bit color, which the C implementation does not support yet.
        Some(&"2") if rest.len() >= 4 => {
            let channel = |f: &str| u8::try_from(sgr_number(f)).ok();
            match (channel(rest[1]), channel(rest[2]), channel(rest[3])) {
                (Some(r), Some(g), Some(b)) => Extended::Color(Color::Rgb(r, g, b), 4),
                _ => Extended::NotUnderstood,
            }
        }
        _ => Extended::NotUnderstood,
    }
}

/// Applies the parameters of an SGR sequence (the digits and semicolons
/// between `ESC [` and `m`).
///
/// `state` is the parser's running state and `applied` the rendition text is
/// written with. They are separate because, as upstream, `applied` only picks
/// up `state` after each code that was understood; the first code that is not
/// understood stops processing of the remaining fields.
pub fn apply_sgr(state: &mut Attr, applied: &mut Attr, params: &str) {
    let fields: Vec<&str> = params.split(';').collect();
    let mut i = 0;
    while i < fields.len() {
        let code = sgr_number(fields[i]);
        let mut consumed = 1;
        match code {
            -1 => {}
            0 => *state = Attr::DEFAULT,
            1 => state.bold = true,
            2 => state.dim = true,
            3 => state.italic = true,
            4 => state.underline = true,
            5 => state.blink = true,
            7 => state.reverse = true,
            21 => state.bold = false,
            22 => {
                state.bold = false;
                state.dim = false;
            }
            23 => state.italic = false,
            24 => state.underline = false,
            25 => state.blink = false,
            27 => state.reverse = false,
            30..=37 => state.fg = Color::Indexed((code - 30) as u8),
            39 => state.fg = Color::Default,
            40..=47 => state.bg = Color::Indexed((code - 40) as u8),
            49 => state.bg = Color::Default,
            90..=97 => state.fg = Color::Indexed((code - 90 + 8) as u8),
            100..=107 => state.bg = Color::Indexed((code - 100 + 8) as u8),
            38 | 48 => {
                let target = if code == 38 {
                    &mut state.fg
                } else {
                    &mut state.bg
                };
                match extended_color(&fields[i + 1..]) {
                    Extended::Color(color, n) => {
                        *target = color;
                        consumed += n;
                    }
                    Extended::NotUnderstood => {
                        *target = Color::Default;
                        return;
                    }
                }
            }
            _ => return,
        }
        *applied = *state;
        i += consumed;
    }
}

fn push_color(out: &mut String, color: Color, base: u8, bright_base: u8, extended: u8) {
    match color {
        Color::Default => {}
        Color::Indexed(n) if n < 8 => {
            let _ = write!(out, ";{}", base + n);
        }
        Color::Indexed(n) if n < 16 => {
            let _ = write!(out, ";{}", bright_base + n - 8);
        }
        Color::Indexed(n) => {
            let _ = write!(out, ";{extended};5;{n}");
        }
        Color::Rgb(r, g, b) => {
            let _ = write!(out, ";{extended};2;{r};{g};{b}");
        }
    }
}

/// The escape sequence that switches the terminal to `attr`, starting from a
/// full reset. Standout is rendered as reverse video, as terminfo's `smso`
/// does on xterm-compatible terminals.
pub fn sgr_sequence(attr: &Attr, standout: bool) -> String {
    let mut out = String::from("\x1b[0");
    if attr.bold {
        out.push_str(";1");
    }
    if attr.dim {
        out.push_str(";2");
    }
    if attr.italic {
        out.push_str(";3");
    }
    if attr.underline {
        out.push_str(";4");
    }
    if attr.blink {
        out.push_str(";5");
    }
    if attr.reverse || standout {
        out.push_str(";7");
    }
    push_color(&mut out, attr.fg, 30, 90, 38);
    push_color(&mut out, attr.bg, 40, 100, 48);
    out.push('m');
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn apply(params: &str) -> (Attr, Attr) {
        let mut state = Attr::DEFAULT;
        let mut applied = Attr::DEFAULT;
        apply_sgr(&mut state, &mut applied, params);
        (state, applied)
    }

    #[test]
    fn empty_parameters_reset() {
        let mut state = Attr {
            bold: true,
            ..Attr::DEFAULT
        };
        let mut applied = state;
        apply_sgr(&mut state, &mut applied, "");
        assert_eq!(applied, Attr::DEFAULT);
    }

    #[test]
    fn basic_and_bright_colors() {
        let (_, a) = apply("1;31;104");
        assert!(a.bold);
        assert_eq!(a.fg, Color::Indexed(1));
        assert_eq!(a.bg, Color::Indexed(12));
    }

    #[test]
    fn empty_field_resets_and_trailing_semicolon_resets() {
        let (_, a) = apply("1;;31");
        assert!(!a.bold);
        assert_eq!(a.fg, Color::Indexed(1));
        let (_, a) = apply("1;");
        assert_eq!(a, Attr::DEFAULT);
    }

    #[test]
    fn indexed_colors_consume_their_fields() {
        let (_, a) = apply("38;5;196;1");
        assert_eq!(a.fg, Color::Indexed(196));
        assert!(a.bold);
    }

    #[test]
    fn unknown_code_stops_processing() {
        let (_, a) = apply("1;99;4");
        assert!(a.bold);
        assert!(!a.underline);
    }

    #[test]
    fn malformed_extended_color_resets_pending_color_only() {
        let (state, applied) = apply("31;38;5");
        assert_eq!(applied.fg, Color::Indexed(1));
        assert_eq!(state.fg, Color::Default);
    }

    #[test]
    fn code_21_only_clears_bold() {
        let (_, a) = apply("1;2;21");
        assert!(!a.bold);
        assert!(a.dim);
    }

    #[test]
    fn truecolor_is_supported() {
        let (_, a) = apply("38;2;255;0;10;48;2;1;2;3");
        assert_eq!(a.fg, Color::Rgb(255, 0, 10));
        assert_eq!(a.bg, Color::Rgb(1, 2, 3));
    }

    #[test]
    fn sequence_uses_terminfo_color_forms() {
        let attr = Attr {
            bold: true,
            fg: Color::Indexed(9),
            bg: Color::Indexed(200),
            ..Attr::DEFAULT
        };
        assert_eq!(sgr_sequence(&attr, true), "\x1b[0;1;7;91;48;5;200m");
    }
}
