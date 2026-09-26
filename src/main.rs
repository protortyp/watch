mod ansi;
mod app;
mod args;
mod command;
mod locale;
mod output;
mod signals;
mod terminal;
mod window;

use std::env;
use std::ffi::OsString;
use std::io::{self, Write};
use std::process;

fn main() {
    locale::init();
    let argv: Vec<OsString> = env::args_os().collect();
    let program = argv
        .first()
        .map(|a| args::program_name(a))
        .unwrap_or_else(|| "watch".to_string());

    let code = match args::parse(&argv, env::var_os("WATCH_INTERVAL").as_deref()) {
        Ok(opts) => match app::run(opts) {
            Ok(code) => code,
            Err(fatal) => {
                eprintln!("{program}: {}", fatal.message);
                fatal.code
            }
        },
        Err(exit) => {
            print!("{}", exit.stdout);
            eprint!("{}", exit.stderr);
            exit.code
        }
    };
    let _ = io::stdout().flush();
    process::exit(code);
}
