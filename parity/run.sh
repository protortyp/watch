#!/bin/sh
# Builds the comparison container if needed, then builds the port inside it
# and runs the side-by-side cases against procps-ng watch.
#
#   parity/run.sh                  all cases
#   parity/run.sh differences -j1  cases whose name contains "differences"
set -eu

here=$(cd "$(dirname "$0")" && pwd)
engine=${CONTAINER_ENGINE:-$(command -v podman || command -v docker)}

if ! "$engine" image exists watch-parity 2>/dev/null; then
    "$engine" build -t watch-parity -f "$here/Containerfile" "$here"
fi

exec "$engine" run --rm --init \
    -v "$here/..:/work:ro" \
    -v watch-parity-target:/target \
    watch-parity sh -c '
        set -e
        CARGO_TARGET_DIR=/target cargo build --release --quiet --manifest-path /work/Cargo.toml
        mkdir -p /opt/upstream/bin /opt/port/bin
        ln -sf /usr/local/bin/watch-upstream /opt/upstream/bin/watch
        ln -sf /target/release/watch /opt/port/bin/watch
        exec python3 /work/parity/harness.py "$@"
    ' harness "$@"
