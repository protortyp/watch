# watch (Rust)

A Rust implementation of `watch` focused on making the tool platform-independent, including macOS support.

This project is modeled after the Linux `watch` implementation from procps-ng:
- https://gitlab.com/procps-ng/procps/-/blob/master/src/watch.c

## Basics

Run a command repeatedly and show its output fullscreen.

```bash
watch -n 1 df -h .
```

Press:
- `q` to quit
- `Ctrl+C` to interrupt
- `space` to run immediately
- `s` to save a screenshot

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
