use std::ffi::{OsStr, OsString};
use std::os::unix::ffi::{OsStrExt, OsStringExt};

use crate::locale::strerror;

pub const MIN_INTERVAL: f64 = 0.1;
pub const MAX_INTERVAL: f64 = 60.0 * 60.0 * 24.0 * 31.0;

#[derive(Debug, Clone, PartialEq)]
pub struct Options {
    pub beep: bool,
    pub color: bool,
    pub differences: bool,
    pub cumulative: bool,
    pub errexit: bool,
    pub follow: bool,
    pub chgexit: bool,
    pub equexit: Option<i64>,
    pub interval: f64,
    pub precise: bool,
    pub no_rerun: bool,
    pub shotsdir: OsString,
    pub no_title: bool,
    pub no_wrap: bool,
    pub exec: bool,
    pub tty: bool,
    pub command_argv: Vec<OsString>,
    /// The command words joined by single spaces, as passed to `sh -c`.
    pub command: OsString,
}

impl Options {
    /// Whether any option needs to know if the screen contents changed.
    pub fn tracks_changes(&self) -> bool {
        self.differences || self.chgexit || self.equexit.is_some()
    }
}

/// How argument parsing ended when it did not produce options to run with.
#[derive(Debug, PartialEq, Eq)]
pub struct Exit {
    pub code: i32,
    pub stdout: String,
    pub stderr: String,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum HasArg {
    No,
    Required,
    Optional,
}

/// Mirrors the upstream `longopts` table; its order determines the order of
/// candidates in glibc's "is ambiguous" message.
const LONG_OPTIONS: &[(&str, HasArg, char)] = &[
    ("color", HasArg::No, 'c'),
    ("no-color", HasArg::No, 'C'),
    ("differences", HasArg::Optional, 'd'),
    ("help", HasArg::No, 'h'),
    ("interval", HasArg::Required, 'n'),
    ("beep", HasArg::No, 'b'),
    ("errexit", HasArg::No, 'e'),
    ("follow", HasArg::No, 'f'),
    ("chgexit", HasArg::No, 'g'),
    ("equexit", HasArg::Required, 'q'),
    ("exec", HasArg::No, 'x'),
    ("precise", HasArg::No, 'p'),
    ("no-rerun", HasArg::No, 'r'),
    ("shotsdir", HasArg::Required, 's'),
    ("no-title", HasArg::No, 't'),
    ("no-wrap", HasArg::No, 'w'),
    ("version", HasArg::No, 'v'),
    ("tty", HasArg::No, TTY_OPTION),
];

/// `--tty` has no short form; this value only identifies it internally.
const TTY_OPTION: char = '\u{1}';

fn short_option(c: char) -> Option<HasArg> {
    match c {
        'b' | 'C' | 'c' | 'e' | 'f' | 'g' | 'h' | 'p' | 'r' | 't' | 'w' | 'v' | 'x' => {
            Some(HasArg::No)
        }
        'd' => Some(HasArg::Optional),
        'q' | 'n' | 's' => Some(HasArg::Required),
        _ => None,
    }
}

pub fn program_name(argv0: &OsStr) -> String {
    let bytes = argv0.as_bytes();
    let short = match bytes.iter().rposition(|&b| b == b'/') {
        Some(i) => &bytes[i + 1..],
        None => bytes,
    };
    String::from_utf8_lossy(short).into_owned()
}

pub fn usage(program: &str) -> String {
    let mut text = format!("\nUsage:\n {program} [options] command\n\nOptions:\n");
    for line in [
        "  -b, --beep             beep if command has a non-zero exit",
        "  -c, --color            interpret ANSI color and style sequences",
        "  -C, --no-color         do not interpret ANSI color and style sequences",
        "  -d, --differences[=<permanent>]",
        "                         highlight changes between updates",
        "  -e, --errexit          exit if command has a non-zero exit",
        "  -f, --follow           Follow the output and don't clear screen",
        "  -g, --chgexit          exit when output from command changes",
        "  -q, --equexit <cycles>",
        "                         exit when output from command does not change",
        "  -n, --interval <secs>  seconds to wait between updates",
        "  -p, --precise          -n includes command running time",
        "  -r, --no-rerun         do not rerun program on window resize",
        "  -s, --shotsdir         directory to store screenshots",
        "  -t, --no-title         turn off header",
        "  -w, --no-wrap          turn off line wrapping",
        "  -x, --exec             pass command to exec instead of \"sh -c\"",
        "      --tty              run command in a pseudo-terminal",
        "",
        " -h, --help     display this help and exit",
        " -v, --version  output version information and exit",
        "",
        "For more details see watch(1).",
    ] {
        text.push_str(line);
        text.push('\n');
    }
    text
}

pub fn version(program: &str) -> String {
    format!("{program} from watch-rs {}\n", env!("CARGO_PKG_VERSION"))
}

/// The `errno` value upstream's number parsers report when they fail
/// without setting it themselves. They print whatever is left over from
/// startup, which with glibc is `ENOENT` from locale initialization.
const STALE_ERRNO: i32 = libc::ENOENT;

/// Parses an interval the way procps' `strtod_nol_or_err` does: optional
/// leading whitespace and sign, digits, and at most one `.` or `,` as the
/// decimal separator regardless of locale. The arithmetic is kept identical
/// so both implementations round the same way.
fn parse_interval(raw: &OsStr, errmesg: &str, program: &str) -> Result<f64, Exit> {
    let s = raw.as_bytes();
    let fail = |errno: Option<i32>| {
        let mut msg = format!("{program}: {errmesg}: '{}'", raw.to_string_lossy());
        if let Some(e) = errno {
            msg.push_str(": ");
            msg.push_str(&strerror(e));
        }
        msg.push('\n');
        Exit {
            code: 1,
            stdout: String::new(),
            stderr: msg,
        }
    };
    if s.is_empty() {
        return Err(fail(Some(STALE_ERRNO)));
    }

    let mut i = 0;
    while i < s.len() && is_c_space(s[i]) {
        i += 1;
    }
    let mut negative = false;
    if i < s.len() && s[i] == b'-' {
        negative = true;
        i += 1;
    } else if i < s.len() && s[i] == b'+' {
        i += 1;
    }

    let mut num = 0.0f64;
    let mut mult = 0.1f64;
    let mut radix = i;
    while radix < s.len() && s[radix].is_ascii_digit() {
        radix += 1;
        mult *= 10.0;
    }
    while i < s.len() && s[i].is_ascii_digit() {
        num += f64::from(s[i] - b'0') * mult;
        mult /= 10.0;
        i += 1;
    }
    let finish = |num: f64| if negative { -num } else { num };
    if i == s.len() {
        return Ok(finish(num));
    }
    if s[i] != b'.' && s[i] != b',' {
        return Err(fail(Some(libc::EINVAL)));
    }
    i += 1;
    mult = 0.1;
    while i < s.len() && s[i].is_ascii_digit() {
        num += f64::from(s[i] - b'0') * mult;
        mult /= 10.0;
        i += 1;
    }
    if i == s.len() {
        return Ok(finish(num));
    }
    Err(fail(Some(STALE_ERRNO)))
}

/// `strtol_or_err` with base 10.
fn parse_long(raw: &OsStr, program: &str) -> Result<i64, Exit> {
    let s = raw.as_bytes();
    let fail = |errno: Option<i32>| {
        let mut msg = format!(
            "{program}: failed to parse argument: '{}'",
            raw.to_string_lossy()
        );
        if let Some(e) = errno {
            msg.push_str(": ");
            msg.push_str(&strerror(e));
        }
        msg.push('\n');
        Exit {
            code: 1,
            stdout: String::new(),
            stderr: msg,
        }
    };

    let mut i = 0;
    while i < s.len() && is_c_space(s[i]) {
        i += 1;
    }
    let mut negative = false;
    if i < s.len() && (s[i] == b'-' || s[i] == b'+') {
        negative = s[i] == b'-';
        i += 1;
    }
    let digits_start = i;
    let mut value: i64 = 0;
    let mut overflow = false;
    while i < s.len() && s[i].is_ascii_digit() {
        let d = i64::from(s[i] - b'0');
        value = match value.checked_mul(10).and_then(|v| {
            if negative {
                v.checked_sub(d)
            } else {
                v.checked_add(d)
            }
        }) {
            Some(v) => v,
            None => {
                overflow = true;
                value
            }
        };
        i += 1;
    }
    if i == digits_start || i != s.len() {
        return Err(fail(None));
    }
    if overflow {
        return Err(fail(Some(libc::ERANGE)));
    }
    Ok(value)
}

fn is_c_space(b: u8) -> bool {
    matches!(b, b' ' | b'\t' | b'\n' | b'\x0b' | b'\x0c' | b'\r')
}

fn usage_error(program: &str, message: String) -> Exit {
    Exit {
        code: 1,
        stdout: String::new(),
        stderr: message + &usage(program),
    }
}

/// Parses the command line with the semantics of glibc
/// `getopt_long(argc, argv, "+bCcefd::ghq:n:prs:twvx", longopts, NULL)`:
/// option processing stops at the first non-option, long options may be
/// abbreviated to any unambiguous prefix, and diagnostics use glibc's wording.
pub fn parse(args: &[OsString], watch_interval: Option<&OsStr>) -> Result<Options, Exit> {
    let argv0 = args
        .first()
        .map(|a| a.to_string_lossy().into_owned())
        .unwrap_or_else(|| "watch".to_string());
    let program = program_name(OsStr::new(&argv0));

    let mut opts = Options {
        beep: false,
        color: false,
        differences: false,
        cumulative: false,
        errexit: false,
        follow: false,
        chgexit: false,
        equexit: None,
        interval: 2.0,
        precise: false,
        no_rerun: false,
        shotsdir: OsString::new(),
        no_title: false,
        no_wrap: false,
        exec: false,
        tty: false,
        command_argv: Vec::new(),
        command: OsString::new(),
    };

    if let Some(raw) = watch_interval {
        opts.interval = parse_interval(
            raw,
            "Could not parse interval from WATCH_INTERVAL",
            &program,
        )?;
    }

    let mut i = 1;
    while i < args.len() {
        let arg = args[i].as_bytes();
        if arg == b"--" {
            i += 1;
            break;
        }
        if arg.len() < 2 || arg[0] != b'-' {
            break;
        }

        if let Some(body) = arg.strip_prefix(b"--") {
            let (name, value) = match body.iter().position(|&b| b == b'=') {
                Some(eq) => (&body[..eq], Some(&body[eq + 1..])),
                None => (body, None),
            };
            let name_str = String::from_utf8_lossy(name);
            let exact = LONG_OPTIONS.iter().find(|(n, _, _)| n.as_bytes() == name);
            let found = match exact {
                Some(opt) => *opt,
                None => {
                    let candidates: Vec<_> = LONG_OPTIONS
                        .iter()
                        .filter(|(n, _, _)| n.as_bytes().starts_with(name))
                        .collect();
                    match candidates.as_slice() {
                        [] => {
                            return Err(usage_error(
                                &program,
                                format!(
                                    "{argv0}: unrecognized option '--{}'\n",
                                    String::from_utf8_lossy(body)
                                ),
                            ));
                        }
                        [only] => **only,
                        many => {
                            let mut msg = format!(
                                "{argv0}: option '--{name_str}' is ambiguous; possibilities:"
                            );
                            for (n, _, _) in many {
                                msg.push_str(&format!(" '--{n}'"));
                            }
                            msg.push('\n');
                            return Err(usage_error(&program, msg));
                        }
                    }
                }
            };
            let (long_name, has_arg, key) = found;
            let optarg: Option<OsString> = match (has_arg, value) {
                (HasArg::No, Some(_)) => {
                    return Err(usage_error(
                        &program,
                        format!("{argv0}: option '--{long_name}' doesn't allow an argument\n"),
                    ));
                }
                (HasArg::No, None) => None,
                (HasArg::Optional, v) => v.map(|v| OsString::from_vec(v.to_vec())),
                (HasArg::Required, Some(v)) => Some(OsString::from_vec(v.to_vec())),
                (HasArg::Required, None) => {
                    i += 1;
                    match args.get(i) {
                        Some(v) => Some(v.clone()),
                        None => {
                            return Err(usage_error(
                                &program,
                                format!("{argv0}: option '--{long_name}' requires an argument\n"),
                            ));
                        }
                    }
                }
            };
            if let Some(exit) = apply(&mut opts, key, optarg, &program)? {
                return Err(exit);
            }
            i += 1;
            continue;
        }

        let cluster = &arg[1..];
        let mut j = 0;
        while j < cluster.len() {
            let c = cluster[j] as char;
            j += 1;
            let Some(has_arg) = short_option(c) else {
                let shown = if cluster[j - 1].is_ascii() {
                    c.to_string()
                } else {
                    String::from_utf8_lossy(&cluster[j - 1..j]).into_owned()
                };
                return Err(usage_error(
                    &program,
                    format!("{argv0}: invalid option -- '{shown}'\n"),
                ));
            };
            let optarg = match has_arg {
                HasArg::No => None,
                HasArg::Optional => {
                    let rest = &cluster[j..];
                    j = cluster.len();
                    (!rest.is_empty()).then(|| OsString::from_vec(rest.to_vec()))
                }
                HasArg::Required => {
                    let rest = &cluster[j..];
                    j = cluster.len();
                    if !rest.is_empty() {
                        Some(OsString::from_vec(rest.to_vec()))
                    } else {
                        i += 1;
                        match args.get(i) {
                            Some(v) => Some(v.clone()),
                            None => {
                                return Err(usage_error(
                                    &program,
                                    format!("{argv0}: option requires an argument -- '{c}'\n"),
                                ));
                            }
                        }
                    }
                }
            };
            if let Some(exit) = apply(&mut opts, c, optarg, &program)? {
                return Err(exit);
            }
        }
        i += 1;
    }

    if i >= args.len() {
        return Err(Exit {
            code: 1,
            stdout: String::new(),
            stderr: usage(&program),
        });
    }

    if opts.follow && opts.tracks_changes() {
        return Err(usage_error(
            &program,
            "Follow -f option conflicts with change options -d,-e or -q".to_string(),
        ));
    }

    opts.command_argv = args[i..].to_vec();
    let mut command = Vec::new();
    for (n, word) in opts.command_argv.iter().enumerate() {
        if n > 0 {
            command.push(b' ');
        }
        command.extend_from_slice(word.as_bytes());
    }
    opts.command = OsString::from_vec(command);
    opts.interval = opts.interval.clamp(MIN_INTERVAL, MAX_INTERVAL);
    Ok(opts)
}

/// Applies one parsed option. `Ok(Some(exit))` ends parsing successfully,
/// as `--help` and `--version` do.
fn apply(
    opts: &mut Options,
    key: char,
    optarg: Option<OsString>,
    program: &str,
) -> Result<Option<Exit>, Exit> {
    match key {
        'b' => opts.beep = true,
        'c' => opts.color = true,
        'C' => opts.color = false,
        'd' => {
            opts.differences = true;
            if optarg.is_some() {
                opts.cumulative = true;
            }
        }
        'e' => opts.errexit = true,
        'f' => opts.follow = true,
        'g' => opts.chgexit = true,
        'q' => {
            let raw = optarg.unwrap_or_default();
            opts.equexit = Some(parse_long(&raw, program)?.max(1));
        }
        'r' => opts.no_rerun = true,
        's' => opts.shotsdir = optarg.unwrap_or_default(),
        't' => opts.no_title = true,
        'w' => opts.no_wrap = true,
        'x' => opts.exec = true,
        'n' => {
            let raw = optarg.unwrap_or_default();
            opts.interval = parse_interval(&raw, "failed to parse argument", program)?;
        }
        'p' => opts.precise = true,
        TTY_OPTION => opts.tty = true,
        'h' => {
            return Ok(Some(Exit {
                code: 0,
                stdout: usage(program),
                stderr: String::new(),
            }));
        }
        'v' => {
            return Ok(Some(Exit {
                code: 0,
                stdout: version(program),
                stderr: String::new(),
            }));
        }
        _ => unreachable!("option table and handler disagree on '{key}'"),
    }
    Ok(None)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(list: &[&str]) -> Vec<OsString> {
        std::iter::once("watch")
            .chain(list.iter().copied())
            .map(OsString::from)
            .collect()
    }

    fn ok(list: &[&str]) -> Options {
        parse(&args(list), None).expect("arguments should parse")
    }

    fn err(list: &[&str]) -> Exit {
        parse(&args(list), None).expect_err("arguments should be rejected")
    }

    fn first_line(s: &str) -> &str {
        s.lines().next().unwrap_or("")
    }

    #[test]
    fn stops_at_first_non_option() {
        let o = ok(&["-n", "5", "ls", "-l", "-d"]);
        assert_eq!(o.interval, 5.0);
        assert!(!o.differences);
        assert_eq!(o.command, OsString::from("ls -l -d"));
    }

    #[test]
    fn double_dash_ends_options() {
        let o = ok(&["--", "-d"]);
        assert!(!o.differences);
        assert_eq!(o.command_argv, vec![OsString::from("-d")]);
    }

    #[test]
    fn short_options_cluster_and_take_attached_values() {
        let o = ok(&["-tbn3", "true"]);
        assert!(o.no_title && o.beep);
        assert_eq!(o.interval, 3.0);
    }

    #[test]
    fn required_argument_may_start_with_dash() {
        let o = ok(&["-n", "-1", "true"]);
        assert_eq!(o.interval, MIN_INTERVAL);
    }

    #[test]
    fn differences_argument_must_be_attached() {
        assert!(ok(&["-d1", "true"]).cumulative);
        assert!(ok(&["--differences=x", "true"]).cumulative);
        let o = ok(&["-d", "1", "true"]);
        assert!(o.differences && !o.cumulative);
        assert_eq!(o.command, OsString::from("1 true"));
    }

    #[test]
    fn long_options_accept_unique_prefixes() {
        let o = ok(&["--int", "4", "--diff", "--no-t", "true"]);
        assert_eq!(o.interval, 4.0);
        assert!(o.differences && o.no_title);
    }

    #[test]
    fn ambiguous_prefix_lists_candidates_in_table_order() {
        let e = err(&["--e", "true"]);
        assert_eq!(e.code, 1);
        assert_eq!(
            first_line(&e.stderr),
            "watch: option '--e' is ambiguous; possibilities: '--errexit' '--equexit' '--exec'"
        );
        assert!(e.stderr.contains("\nUsage:\n watch [options] command\n"));
    }

    #[test]
    fn getopt_diagnostics_match_glibc() {
        assert_eq!(
            first_line(&err(&["-z"]).stderr),
            "watch: invalid option -- 'z'"
        );
        assert_eq!(
            first_line(&err(&["--foo=1", "x"]).stderr),
            "watch: unrecognized option '--foo=1'"
        );
        assert_eq!(
            first_line(&err(&["--beep=1", "x"]).stderr),
            "watch: option '--beep' doesn't allow an argument"
        );
        assert_eq!(
            first_line(&err(&["--int"]).stderr),
            "watch: option '--interval' requires an argument"
        );
        assert_eq!(
            first_line(&err(&["-n"]).stderr),
            "watch: option requires an argument -- 'n'"
        );
    }

    #[test]
    fn interval_parsing_matches_procps() {
        assert_eq!(ok(&["-n", "1,5", "x"]).interval, 1.5);
        assert_eq!(ok(&["-n", " +2.", "x"]).interval, 2.0);
        assert_eq!(ok(&["-n", ".5", "x"]).interval, 0.5);
        assert_eq!(ok(&["-n", "99999999", "x"]).interval, MAX_INTERVAL);
        assert_eq!(
            err(&["-n", "abc", "x"]).stderr,
            "watch: failed to parse argument: 'abc': Invalid argument\n"
        );
        let stale = strerror(STALE_ERRNO);
        assert_eq!(
            err(&["-n", "1.5x", "x"]).stderr,
            format!("watch: failed to parse argument: '1.5x': {stale}\n")
        );
        assert_eq!(
            err(&["-n", "", "x"]).stderr,
            format!("watch: failed to parse argument: '': {stale}\n")
        );
        assert!(
            err(&["-n", "1e3", "x"])
                .stderr
                .ends_with("'1e3': Invalid argument\n")
        );
    }

    #[test]
    fn watch_interval_environment_is_validated_first() {
        let e = parse(&args(&["-n", "3", "x"]), Some(OsStr::new("x"))).unwrap_err();
        assert_eq!(
            e.stderr,
            "watch: Could not parse interval from WATCH_INTERVAL: 'x': Invalid argument\n"
        );
        let o = parse(&args(&["x"]), Some(OsStr::new("7"))).unwrap();
        assert_eq!(o.interval, 7.0);
    }

    #[test]
    fn equexit_is_clamped_and_strictly_parsed() {
        assert_eq!(ok(&["-q", "-4", "x"]).equexit, Some(1));
        assert_eq!(ok(&["-q", " 3", "x"]).equexit, Some(3));
        assert_eq!(
            err(&["-q", "x", "y"]).stderr,
            "watch: failed to parse argument: 'x'\n"
        );
        assert_eq!(
            err(&["-q", "99999999999999999999", "y"]).stderr,
            format!(
                "watch: failed to parse argument: '99999999999999999999': {}\n",
                strerror(libc::ERANGE)
            )
        );
    }

    #[test]
    fn follow_conflicts_with_change_tracking() {
        let e = err(&["-f", "-g", "true"]);
        assert!(
            e.stderr
                .starts_with("Follow -f option conflicts with change options -d,-e or -q\nUsage:")
        );
    }

    #[test]
    fn help_and_version_stop_parsing_immediately() {
        let e = err(&["-h", "-z"]);
        assert_eq!(e.code, 0);
        assert!(e.stdout.starts_with("\nUsage:\n"));
        let e = err(&["--vers"]);
        assert_eq!(e.code, 0);
        assert!(e.stdout.starts_with("watch from watch-rs "));
    }

    #[test]
    fn missing_command_prints_usage_to_stderr() {
        let e = err(&["-n", "2"]);
        assert_eq!(e.code, 1);
        assert_eq!(e.stderr, usage("watch"));
    }

    #[test]
    fn diagnostics_use_full_argv0_but_usage_uses_basename() {
        let argv = vec![OsString::from("/usr/bin/watch"), OsString::from("-z")];
        let e = parse(&argv, None).unwrap_err();
        assert!(
            e.stderr
                .starts_with("/usr/bin/watch: invalid option -- 'z'\n")
        );
        assert!(e.stderr.contains("\n watch [options] command\n"));
    }
}
