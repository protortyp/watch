#!/usr/bin/env python3
"""Runs procps-ng watch and this port side by side and compares them.

Each case starts both implementations in their own tmux sessions of the same
size, drives them through the same steps (waits, keys, resizes, signals) at
the same moments, and compares every screen capture (including colors), the
exit status, stderr and any screenshot files. Header clocks are frozen with
libfaketime; command run times are masked since they legitimately differ.

Run inside the container built from parity/Containerfile; see parity/run.sh.
"""

import argparse
import concurrent.futures
import difflib
import os
import re
import shlex
import subprocess
import sys
import tempfile
import time
from dataclasses import dataclass, field

BIN_DIRS = {"upstream": "/opt/upstream/bin", "port": "/opt/port/bin"}
FAKETIME = "2026-03-04 05:06:07"
BASE_ENV = {
    "LANG": "en_US.UTF-8",
    "TZ": "UTC",
    "LD_PRELOAD": "/usr/lib/aarch64-linux-gnu/faketime/libfaketime.so.1",
    "FAKETIME": FAKETIME,
    "FAKETIME_DONT_FAKE_MONOTONIC": "1",
    "FAKETIME_DONT_RESET": "1",
}

LONG_COMMAND = "echo some long command here"


@dataclass
class Case:
    name: str
    args: list
    width: int = 50
    height: int = 8
    env: dict = field(default_factory=dict)
    steps: list = field(default_factory=lambda: [("sleep", 0.8), ("capture",)])
    setup: str = ""
    stdin: str | None = None
    shell: bool = False
    # libfaketime occasionally changes upstream's exit status when it exits
    # right after forking; cases that never show the clock can do without it.
    faketime: bool = True


def cmd(script):
    """A shell command line, run through watch's default `sh -c` mode."""
    return script


def seq(*steps):
    return list(steps)


CASES = [
    Case("plain", ["-n", "5", cmd("echo hello; echo world")]),
    Case("no-title", ["-t", "-n", "5", cmd("printf 'a\\nb\\n'")]),
    *[
        Case(f"header-width-{w}", ["-n", "7", cmd(LONG_COMMAND)], width=w, height=4)
        for w in (80, 72, 70, 60, 58, 53, 52, 45, 44, 30)
    ],
    Case("header-unicode-command", ["-n", "7", cmd("echo 日本語 résumé")], width=80, height=4),
    Case("header-tab-command", ["-n", "7", cmd("echo a\tb")], width=80, height=4),
    Case("interval-clamped", ["-n", "0.01", "-q", "100", cmd("echo x")], width=80, height=4),
    Case("interval-env", [cmd("echo x")], env={"WATCH_INTERVAL": "3,25"}, width=80, height=4),
    Case(
        "locale-de",
        ["-n", "2.5", cmd("echo x")],
        env={"LANG": "de_DE.UTF-8"},
        width=80,
        height=4,
    ),
    Case(
        "colors",
        [
            "-t",
            "-c",
            "-n",
            "9",
            cmd(
                "printf '\\033[1;31mR\\033[0m \\033[92mG\\033[m \\033[38;5;196mX\\033[38;5;9mY"
                "\\033[48;5;21mZ\\033[0m\\n\\033(B\\033[mplain \\033[4;7mUR\\033[m \\033[3mI\\033[0m"
                " \\033[2mD\\033[22mN \\033[1;2;21mQ\\033[0m\\n\\033[31mred-to-eol\\n"
                "\\033[1;99;4mQ\\033[0m \\033[31;38;5mK\\033[1mL\\033[0m \\033[44;;32mE\\033[0m\\n'"
            ),
        ],
    ),
    Case(
        "no-color-escapes",
        ["-t", "-n", "9", cmd("printf '\\033[31mred\\033[0m \\033(B\\033[m x\\n'")],
    ),
    Case(
        "other-escapes",
        ["-t", "-c", "-n", "9", cmd("printf 'a\\033[2Kb\\033[?25lc\\033]0;t\\007d\\033Me\\n'")],
    ),
    Case(
        "wide-and-combining",
        [
            "-t",
            "-n",
            "9",
            cmd(
                "printf 'e\\314\\201z|\\n\\346\\227\\245\\346\\234\\254|\\n"
                "0123456789012345678901234567890123456789012345\\346\\227\\245\\346\\234\\254\\n"
                "\\314\\201start\\n\\360\\237\\230\\200 emoji\\n'"
            ),
        ],
    ),
    Case("tabs", ["-t", "-n", "9", cmd("printf 'a\\tb\\tc\\n1234567\\n12345678\\nx\\ty\\n\\t\\t\\tz\\n'")], width=12),
    Case("control-chars", ["-t", "-n", "9", cmd("printf 'a\\rb\\001c\\177d\\be\\n'")]),
    Case(
        "no-wrap",
        ["-t", "-w", "-c", "-n", "9", cmd("printf '\\033[31m0123456789ABCDEFGHIJ\\nnext\\n\\346\\227\\245\\346\\227\\245\\346\\227\\245\\346\\227\\245\\346\\227\\245\\346\\227\\245\\n'")],
        width=10,
        height=4,
    ),
    Case("wrap-exact-width", ["-t", "-n", "9", cmd("printf '0123456789\\nnext\\n0123456789abc\\n'")], width=10),
    Case("truncated-output", ["-n", "9", cmd("seq 100")]),
    Case("truncated-output-bottom-wrap", ["-t", "-n", "9", cmd("seq 5; printf 'abcdefghijklmnopqrstuvwxyz\\n'")], width=10, height=6),
    Case("invalid-utf8-lead", ["-t", "-n", "9", cmd("printf 'a\\377b\\n\\300\\200c\\n'")]),
    Case("invalid-utf8-sequence", ["-t", "-n", "9", cmd("printf 'ok\\n\\342Ax\\nmore\\n'")]),
    Case("c-locale", ["-t", "-n", "9", cmd("printf 'a\\303\\251b\\nnext\\n'")], env={"LANG": "C"}),
    Case("c-locale-header", ["-n", "9", cmd("echo é")], env={"LANG": "C"}, width=80, height=4),
    Case(
        "differences",
        ["-d", "-n", "0.5", cmd("n=$(cat n 2>/dev/null || echo 7); echo $((n+13)) > n; echo value $n; echo stable")],
        steps=seq(("sleep", 0.25), ("capture",), ("sleep", 0.5), ("capture",), ("sleep", 0.5), ("capture",)),
    ),
    Case(
        "differences-permanent",
        ["-t", "-d1", "-n", "0.5", cmd("n=$(cat n 2>/dev/null || echo 0); echo $((n+1)) > n; case $n in 0) echo abcd;; 1) echo aXcd;; *) echo abcY;; esac")],
        steps=seq(("sleep", 0.25), ("capture",), ("sleep", 0.5), ("capture",), ("sleep", 0.5), ("capture",), ("sleep", 0.5), ("capture",)),
    ),
    Case(
        "differences-color",
        ["-t", "-c", "-d", "-n", "0.5", cmd("n=$(cat n 2>/dev/null || echo 10); echo $((n+11)) > n; printf '\\033[32m%s\\033[0m end\\n' $n")],
        steps=seq(("sleep", 0.25), ("capture",), ("sleep", 0.5), ("capture",)),
    ),
    Case(
        "differences-wide",
        ["-t", "-d", "-n", "0.5", cmd("if [ -e f ]; then printf 'a\\346\\227\\245a\\n'; else printf '\\346\\227\\245aa\\n'; touch f; fi")],
        steps=seq(("sleep", 0.25), ("capture",), ("sleep", 0.5), ("capture",)),
    ),
    Case(
        "differences-colored-clear",
        ["-t", "-c", "-d", "-n", "9", cmd("printf 'x\\033[41m\\n'")],
    ),
    Case(
        "chgexit",
        ["-g", "-n", "0.3", cmd("n=$(cat n 2>/dev/null || echo 0); echo $((n+1)) > n; [ $n -lt 2 ] && echo same || echo changed")],
        steps=seq(("wait_exit", 5),),
    ),
    Case("equexit", ["-q", "3", "-n", "0.2", cmd("echo same")], steps=seq(("wait_exit", 5),)),
    Case(
        "errexit-first-run",
        ["-e", "-n", "0.5", cmd("echo line1; exit 3")],
        steps=seq(("sleep", 0.8), ("capture",), ("keys", "x"), ("wait_exit", 3)),
    ),
    Case(
        "errexit-later-run",
        ["-e", "-n", "0.5", cmd("if [ -e f ]; then echo second; exit 4; else echo first; touch f; fi")],
        steps=seq(("sleep", 0.25), ("capture",), ("sleep", 0.7), ("capture",), ("keys", "q"), ("wait_exit", 3)),
    ),
    Case(
        "errexit-ctrl-c",
        ["-e", "-n", "0.5", cmd("exit 5")],
        steps=seq(("sleep", 0.5), ("keys", "C-c"), ("wait_exit", 3)),
    ),
    Case(
        "follow",
        ["-t", "-f", "-n", "0.5", cmd("printf 0123456789012345678901234567890123456789; echo; echo short")],
        width=40,
        steps=seq(("sleep", 1.25), ("capture",)),
    ),
    Case(
        "follow-no-newline",
        ["-t", "-f", "-n", "0.5", cmd("printf 'ab'")],
        width=10,
        steps=seq(("sleep", 1.25), ("capture",)),
    ),
    Case(
        "follow-with-header",
        ["-f", "-n", "0.5", cmd("echo run")],
        width=40,
        steps=seq(("sleep", 1.25), ("capture",)),
    ),
    Case(
        "screenshot",
        ["-n", "9", cmd("printf 'one\\ntwo\\n'")],
        steps=seq(("sleep", 0.6), ("keys", "s"), ("sleep", 0.3), ("keys", "s"), ("sleep", 0.3), ("capture",)),
    ),
    Case(
        "screenshot-unterminated",
        ["-t", "-n", "9", cmd("printf 'one\\ntwo'")],
        steps=seq(("sleep", 0.6), ("keys", "s"), ("sleep", 0.3)),
    ),
    Case(
        "screenshot-full-diff",
        ["-t", "-d", "-n", "9", cmd("seq 20")],
        steps=seq(("sleep", 0.6), ("keys", "s"), ("sleep", 0.3)),
    ),
    Case(
        "screenshot-per-sleep",
        ["-t", "-n", "0.5", cmd("echo x")],
        steps=seq(("sleep", 0.2), ("keys", "s"), ("sleep", 0.5), ("keys", "s"), ("sleep", 0.5), ("keys", "s"), ("sleep", 0.3)),
    ),
    Case(
        "screenshot-shotsdir",
        ["-t", "-s", "shots", "-n", "9", cmd("printf 'a\\tb\\346\\227\\245\\n'")],
        setup="mkdir shots",
        steps=seq(("sleep", 0.6), ("keys", "s"), ("sleep", 0.3)),
    ),
    Case(
        "screenshot-bad-dir",
        ["-t", "-s", "missing", "-n", "9", cmd("echo x")],
        steps=seq(("sleep", 0.6), ("keys", "s"), ("wait_exit", 3)),
    ),
    Case("quit", ["-n", "20", cmd("echo x")], steps=seq(("sleep", 0.5), ("keys", "q"), ("wait_exit", 3))),
    Case(
        "keys-during-run",
        ["-t", "-n", "20", cmd("sleep 0.6; cat n 2>/dev/null; echo run >> n")],
        steps=seq(("sleep", 0.9), ("keys", " "), ("sleep", 0.2), ("keys", "q"), ("sleep", 0.2), ("capture",), ("wait_exit", 3)),
    ),
    Case(
        "space-reruns",
        ["-t", "-n", "20", cmd("cat n 2>/dev/null; echo run >> n")],
        steps=seq(("sleep", 0.5), ("keys", " "), ("sleep", 0.4), ("capture",)),
    ),
    Case("ctrl-c", ["-n", "20", cmd("echo x")], steps=seq(("sleep", 0.5), ("keys", "C-c"), ("wait_exit", 3))),
    Case(
        "ctrl-c-during-run",
        ["-n", "20", cmd("sleep 5")],
        steps=seq(("sleep", 0.5), ("keys", "C-c"), ("wait_exit", 3)),
    ),
    Case("sigterm", ["-n", "20", cmd("echo x")], steps=seq(("sleep", 0.5), ("signal", "TERM"), ("wait_exit", 3))),
    Case("sighup", ["-n", "20", cmd("echo x")], steps=seq(("sleep", 0.5), ("signal", "HUP"), ("wait_exit", 3))),
    Case(
        "resize-reruns",
        ["-n", "3", cmd("sleep 0.5; echo run; cat n 2>/dev/null; echo again >> n")],
        width=40,
        steps=seq(("sleep", 0.8), ("resize", 30, 6), ("sleep", 0.25), ("capture",), ("sleep", 0.7), ("capture",)),
    ),
    Case(
        "resize-no-rerun",
        ["-r", "-n", "5", cmd("echo run")],
        width=40,
        steps=seq(("sleep", 0.6), ("resize", 30, 6), ("sleep", 0.5), ("capture",)),
    ),
    Case(
        "resize-grow-differences",
        ["-t", "-d", "-n", "0.5", cmd("n=$(cat n 2>/dev/null || echo 0); echo $((n+1)) > n; echo v$n; echo fixed")],
        width=20,
        height=4,
        steps=seq(("sleep", 0.6), ("resize", 30, 6), ("sleep", 0.25), ("capture",), ("sleep", 0.5), ("capture",)),
    ),
    Case("exec-failure", ["-x", "-n", "9", "nonexistent-cmd", "arg"], width=40),
    Case("exec-args", ["-t", "-x", "-n", "9", "printf", "%s|", "a b", "c"]),
    Case("shell-failure", ["-n", "9", cmd("nonexistent-cmd")]),
    Case("signal-exit-code", ["-n", "9", cmd("kill -9 $$")]),
    Case("exit-code", ["-n", "9", cmd("exit 300")]),
    Case("environment", ["-t", "-n", "9", cmd("echo $LINES $COLUMNS; tput cols; tput lines")]),
    Case(
        "environment-no-tty-stdin",
        ["-t", "-n", "9", cmd("echo $LINES $COLUMNS")],
        env={"LINES": "5", "COLUMNS": "20"},
        stdin="/dev/null",
        faketime=False,
        steps=seq(("wait_exit", 3),),
    ),
    Case("stdin-inherited", ["-t", "-n", "9", cmd("[ -t 0 ] && echo tty || echo notty")]),
    Case("beep-on-failure", ["-b", "-n", "0.5", cmd("false")], steps=seq(("sleep", 0.8), ("bell",))),
    Case("no-beep-on-success", ["-b", "-n", "0.5", cmd("true")], steps=seq(("sleep", 0.8), ("bell",))),
    Case("no-beep-without-flag", ["-n", "0.5", cmd("false")], steps=seq(("sleep", 0.8), ("bell",))),
    Case("bell-in-output", ["-t", "-n", "5", cmd("printf 'a\\007b\\n'")], steps=seq(("sleep", 0.8), ("bell",), ("capture",))),
    Case(
        "precise",
        ["-t", "-p", "-n", "1", cmd("echo run >> n; sleep 0.6; wc -l < n")],
        steps=seq(("sleep", 2.9), ("capture",)),
    ),
    Case(
        "not-precise",
        ["-t", "-n", "1", cmd("echo run >> n; sleep 0.6; wc -l < n")],
        steps=seq(("sleep", 2.9), ("capture",)),
    ),
    Case(
        "suspend-resume",
        ["-n", "20", cmd("echo x")],
        shell=True,
        steps=seq(("sleep", 0.6), ("keys", "C-z"), ("sleep", 0.4), ("capture",), ("keys", "fg"), ("keys", "Enter"), ("sleep", 0.5), ("capture",), ("keys", "q"), ("sleep", 0.3), ("capture",)),
    ),
]

# Invocations that never reach the screen: compared on stdout, stderr and
# exit status.
ARG_CASES = [
    ["-h"],
    ["--help", "x"],
    [],
    ["-n"],
    ["-n", "abc", "x"],
    ["-n", "1.5x", "x"],
    ["-n", "", "x"],
    ["-n", "1e3", "x"],
    ["-q", "x", "y"],
    ["-q", "99999999999999999999", "y"],
    ["-q"],
    ["--e", "x"],
    ["--no", "x"],
    ["--n", "x"],
    ["--foo=1", "x"],
    ["--beep=1", "x"],
    ["--interval"],
    ["--int"],
    ["-z"],
    ["-f", "-d", "x"],
    ["-f", "-q", "1", "x"],
    ["-h", "-z"],
    ["-z", "-h"],
    ["-s"],
]
ARG_ENV_CASES = [({"WATCH_INTERVAL": "x"}, ["-n", "3", "x"]), ({"WATCH_INTERVAL": ""}, ["x"])]

# Lines that document behaviour only the port has.
PORT_ONLY_HELP = re.compile(r"^      --tty .*\n", re.M)

DURATION = re.compile(r"^ *in (?:<|>)?[0-9.,]+ ?(?:s|day) \((\d+)\)$")


def normalize_screen(text):
    lines = []
    for line in text.split("\n"):
        plain = re.sub(r"\x1b\[[0-9;]*m", "", line)
        m = DURATION.match(plain)
        lines.append(f"in #s ({m.group(1)})" if m else line.rstrip())
    return "\n".join(lines).rstrip("\n")


def tmux(*args, check=True):
    return subprocess.run(["tmux", *args], capture_output=True, text=True, check=check)


def run_side(case, impl, workdir, results, barrier):
    try:
        _run_side(case, impl, workdir, results, barrier)
    except Exception as e:
        barrier.abort()
        results[impl] = {"error": repr(e)}


def _run_side(case, impl, workdir, results, barrier):
    session = f"{case.name}-{impl}"
    env = {**BASE_ENV, **case.env}
    if not case.faketime:
        for key in [k for k in env if "FAKETIME" in k or k == "LD_PRELOAD"]:
            del env[key]
    env_words = " ".join(f"{k}={shlex.quote(v)}" for k, v in env.items())
    redirect = f" < {shlex.quote(case.stdin)}" if case.stdin else ""
    watch = "watch " + " ".join(shlex.quote(a) for a in case.args)
    path = f"{BIN_DIRS[impl]}:/usr/local/bin:/usr/bin:/bin"
    if case.shell:
        start = f"cd {workdir} && env PATH={path} PS1='$ ' {env_words} bash --norc --noprofile -i"
    else:
        start = f"cd {workdir} && exec env PATH={path} {env_words} {watch} 2>stderr{redirect}"
    tmux("new-session", "-d", "-s", session, "-x", str(case.width), "-y", str(case.height), "sh", "-c", start)
    # tmux only flags bells in windows that are not being looked at.
    tmux("new-window", "-d", "-t", session)
    if case.shell:
        time.sleep(0.3)
        tmux("send-keys", "-t", session, "-l", f"{watch} 2>stderr")
        tmux("send-keys", "-t", session, "Enter")

    out = {"captures": [], "exit": None}
    barrier.wait()
    for step in case.steps:
        kind = step[0]
        if kind == "sleep":
            time.sleep(step[1])
        elif kind == "capture":
            out["captures"].append(normalize_screen(tmux("capture-pane", "-p", "-e", "-t", session).stdout))
        elif kind == "bell":
            flag = tmux("display-message", "-p", "-t", f"{session}:0", "#{window_bell_flag}").stdout.strip()
            out["captures"].append(f"bell={flag}")
        elif kind == "keys":
            key = step[1]
            if len(key) == 1:
                tmux("send-keys", "-t", session, "-l", key)
            else:
                tmux("send-keys", "-t", session, key)
        elif kind == "resize":
            tmux("resize-window", "-t", session, "-x", str(step[1]), "-y", str(step[2]))
        elif kind == "signal":
            pid = tmux("display-message", "-p", "-t", session, "#{pane_pid}").stdout.strip()
            os.kill(int(pid), getattr(__import__("signal"), "SIG" + step[1]))
        elif kind == "wait_exit":
            deadline = time.time() + step[1]
            while time.time() < deadline:
                dead = tmux("display-message", "-p", "-t", session, "#{pane_dead} #{pane_dead_status}").stdout.split()
                if dead and dead[0] == "1":
                    out["exit"] = int(dead[1]) if len(dead) > 1 else None
                    break
                time.sleep(0.05)
            else:
                out["exit"] = "still running"
            out["captures"].append(normalize_screen(tmux("capture-pane", "-p", "-e", "-t", session).stdout))
        barrier.wait()

    # Read stderr before tearing the session down: a hangup can make either
    # implementation report a read error on its way out.
    stderr_path = os.path.join(workdir, "stderr")
    out["stderr"] = open(stderr_path, errors="replace").read() if os.path.exists(stderr_path) else ""
    tmux("kill-session", "-t", session, check=False)
    shots = {}
    for root, _, files in os.walk(workdir):
        for f in files:
            if f.startswith("watch_"):
                rel = os.path.relpath(os.path.join(root, f), workdir)
                shots[rel] = open(os.path.join(root, f), "rb").read().decode("utf-8", "replace")
    out["screenshots"] = shots
    results[impl] = out


SHOW = False


def run_case(case):
    import threading

    barrier = threading.Barrier(2)
    results = {}
    threads = []
    dirs = {}
    for impl in BIN_DIRS:
        d = tempfile.mkdtemp(prefix=f"{case.name}-")
        if case.setup:
            subprocess.run(["sh", "-c", case.setup], cwd=d, check=True)
        dirs[impl] = d
        t = threading.Thread(target=run_side, args=(case, impl, d, results, barrier))
        threads.append(t)
        t.start()
    for t in threads:
        t.join()
    if SHOW:
        for impl, result in results.items():
            print(f"--- {case.name} [{impl}] exit={result.get('exit')!r}")
            for capture in result.get("captures", []):
                print(capture)
    return compare(case.name, results.get("upstream"), results.get("port"))


def compare(name, up, port):
    if up is None or port is None:
        return name, ["harness failure: a side did not finish"]
    if "error" in up or "error" in port:
        return name, [f"harness failure: upstream={up.get('error')} port={port.get('error')}"]
    problems = []
    for key in ("exit", "stderr", "screenshots"):
        if up[key] != port[key]:
            problems.append(f"{key}: upstream={up[key]!r} port={port[key]!r}")
    if len(up["captures"]) != len(port["captures"]):
        problems.append("different number of captures")
    for i, (a, b) in enumerate(zip(up["captures"], port["captures"])):
        if a != b:
            diff = "\n".join(
                difflib.unified_diff(
                    a.split("\n"), b.split("\n"), "upstream", "port", lineterm="", n=1
                )
            )
            problems.append(f"capture {i} differs:\n{diff}")
    return name, problems


def run_direct(impl, args, env=None):
    full_env = {**os.environ, **BASE_ENV, **(env or {}), "PATH": f"{BIN_DIRS[impl]}:/usr/bin:/bin"}
    p = subprocess.run(["watch", *args], capture_output=True, text=True, env=full_env, stdin=subprocess.DEVNULL, timeout=10)
    return {"stdout": PORT_ONLY_HELP.sub("", p.stdout), "stderr": PORT_ONLY_HELP.sub("", p.stderr), "exit": p.returncode}


def run_arg_cases():
    failures = []
    for env, args in [({}, a) for a in ARG_CASES] + ARG_ENV_CASES:
        up = run_direct("upstream", args, env)
        port = run_direct("port", args, env)
        label = f"args {env or ''}{args}"
        if up != port:
            failures.append((label, [f"upstream={up!r}", f"port={port!r}"]))
        else:
            print(f"PASS {label}")
    return failures


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("filter", nargs="*", help="only run cases whose name contains one of these")
    parser.add_argument("-j", "--jobs", type=int, default=4)
    parser.add_argument("--repeat", type=int, default=1, help="run each case this many times")
    parser.add_argument("--show", action="store_true", help="print every capture")
    args = parser.parse_args()
    global SHOW
    SHOW = args.show

    tmux("-f", "/dev/null", "start-server", check=False)
    tmux("set-option", "-g", "default-terminal", "tmux-256color", check=False)
    tmux("new-session", "-d", "-s", "keepalive", check=False)
    tmux("set-option", "-g", "default-terminal", "tmux-256color", check=False)
    tmux("set-option", "-g", "remain-on-exit", "on", check=False)

    cases = [c for c in CASES if not args.filter or any(f in c.name for f in args.filter)]
    failures = [] if args.filter else run_arg_cases()
    with concurrent.futures.ThreadPoolExecutor(max_workers=args.jobs) as pool:
        for name, problems in pool.map(run_case, cases * args.repeat):
            if problems:
                failures.append((name, problems))
                print(f"FAIL {name}")
            else:
                print(f"PASS {name}")

    for name, problems in failures:
        print(f"\n=== {name}")
        for p in problems:
            print(p)
    print(f"\n{len(failures)} failing")
    sys.exit(1 if failures else 0)


if __name__ == "__main__":
    main()
