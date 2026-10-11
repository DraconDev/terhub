#!/usr/bin/env bash
# Verifies that `make build` actually installs the new binary to
# ~/.cargo/bin/terhub. This is the regression test for the
# stale-binary footgun (the working tree's target/debug/terhub
# was fresh but ~/.cargo/bin/terhub was stale, leading to
# "is it broken or is the binary old?" debugging).
#
# Approach: capture the SHA of ~/.cargo/bin/terhub, force a rebuild
# by touching a source file, run `make build`, then verify the SHA
# changed. The script is idempotent: it restores the source file's
# mtime via a sentinel comment in a CI-runnable way.

set -euo pipefail

cd "$(dirname "$0")/.."

CARGO_BIN="${CARGO_HOME:-$HOME/.cargo}/bin/terhub"
if [ ! -x "$CARGO_BIN" ]; then
    echo "FAIL: $CARGO_BIN not found or not executable"
    exit 1
fi

SENTINEL="src/agent.rs"
if [ ! -f "$SENTINEL" ]; then
    echo "FAIL: sentinel file $SENTINEL missing"
    exit 1
fi

# A comment line we can append/remove to force a rebuild without
# changing behavior. The build system only checks the file's
# contents, so we add a no-op line at the top.
MARKER="// check-install sentinel: $(date +%s) $$"

before_sha=$(sha256sum "$CARGO_BIN" | cut -d' ' -f1)
echo "before: $before_sha"

# Force a rebuild: append a sentinel comment, run make build, then
# remove the sentinel so the source is back to its original shape.
trap 'sed -i "\|^// check-install sentinel:|d" "$SENTINEL" 2>/dev/null || true' EXIT
echo "$MARKER" >> "$SENTINEL"

make build >/dev/null 2>&1

after_sha=$(sha256sum "$CARGO_BIN" | cut -d' ' -f1)
echo "after:  $after_sha"

if [ "$before_sha" = "$after_sha" ]; then
    echo "FAIL: $CARGO_BIN SHA did not change after make build"
    echo "      the install step is broken or skipped"
    exit 1
fi

# Verify the new binary actually runs.
"$CARGO_BIN" --help >/dev/null

echo "OK: $CARGO_BIN was reinstalled by 'make build'"
