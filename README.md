# watch (Rust)

A Rust implementation of `watch` that behaves like the Linux one, including on macOS.

It follows the procps-ng implementation (https://gitlab.com/procps-ng/procps/-/blob/master/src/watch.c)
down to its quirks: same options and diagnostics, same screen layout, same change
highlighting, screenshots and exit statuses. A side-by-side test suite checks this against
the real procps-ng `watch` (see [Compatibility testing](#compatibility-testing)).

## Basics

Run a command repeatedly and show its output fullscreen.

```bash
watch -n 1 df -h .
watch -d ls -l
watch -n1 'grep "^cpu MHz" /proc/cpuinfo | sort -nrk4'
```

The command runs through `sh -c` (or directly with `-x`) with its output and errors going
to watch. `LINES` and `COLUMNS` are set to the terminal size, so tools that size their
output from them fit the screen.

Keys:
- `q` quits (after the current run finishes)
- `space` runs the command right away
- `s` saves a screenshot to `watch_<date>-<time>` in the current directory or `--shotsdir`
- `Ctrl+C` exits immediately
- `Ctrl+Z` suspends watch together with the command

Keys pressed while the command runs are handled once it has finished.

Exit status is 0 unless something went wrong (1 for usage and terminal errors, 2 when the
command cannot be started), or with `--errexit`, the status of the failing command.

## Differences from procps-ng

- `--tty` runs the command in a pseudo-terminal the size of the output area. Programs that
  only color or lay out their output for terminals then do so without extra flags
  (`watch -c --tty ls` instead of `watch -c ls --color=always`). `PAGER` and `GIT_PAGER`
  are set to `cat` so that nothing waits for a pager.
- 24-bit color sequences (`ESC[38;2;r;g;bm`) are shown with `-c`; procps-ng does not
  implement them yet and resets the color instead.
- `--version` reports this implementation.

## Install

If Rust/Cargo is set up correctly, install this binary into your Cargo bin directory:

```bash
cargo install --path .
```

This installs `watch` to `~/.cargo/bin/watch` by default.
If your PATH is set up properly (for example includes `~/.cargo/bin`), you can run:

```bash
watch --help
```

## Compatibility testing

`parity/run.sh` builds a Linux container with procps-ng `watch` compiled from source, then
runs both implementations side by side in tmux: same terminal size, same keys, resizes and
signals at the same moments. It compares the screen contents including colors, exit
status, stderr and screenshot files. It needs podman or docker.

```bash
parity/run.sh                 # everything
parity/run.sh differences     # only cases whose name contains "differences"
parity/run.sh --show header   # print what both implementations displayed
```

Unit tests run with `cargo test`.
