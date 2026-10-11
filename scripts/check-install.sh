#!/usr/bin/env bash
# Verifies that `make build` actually installs the new binary to
# ~/.cargo/bin/terhub. This is the regression test for the
# stale-binary footgun: the working tree's target/debug/terhub
# was fresh but ~/.cargo/bin/terhub was stale, leading to
# "is it broken or is the binary old?" debugging.
#
# Approach: record the mtime of ~/.cargo/bin/terhub, force a
# rebuild by touching a source file (cargo's incremental cache
# is keyed on mtime, so any mtime bump triggers a rebuild), run
# `make build`, then verify the mtime of ~/.cargo/bin/terhub
# advanced. We compare mtime rather than SHA because adding a
# comment to a Rust source compiles to a bit-identical binary
# (comments are stripped before AST) — install still runs and
# updates mtime, but the content is unchanged, so SHA is the
# wrong signal. mtime is the canonical "the install step ran"
# proof.

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

before_mtime=$(stat -c %Y "$CARGO_BIN")
echo "before: mtime=$before_mtime"

# Force a rebuild: touch updates the source mtime, so cargo's
# incremental cache marks it dirty and recompiles. The release
# profile produced by `cargo install` then overwrites
# ~/.cargo/bin/terhub, refreshing its mtime even when the binary
# is bit-identical (which happens when the source change is a
# comment-only edit).
touch "$SENTINEL"
make build >/dev/null 2>&1

after_mtime=$(stat -c %Y "$CARGO_BIN")
echo "after:  mtime=$after_mtime"

if [ "$after_mtime" -le "$before_mtime" ]; then
    echo "FAIL: $CARGO_BIN mtime did not advance after make build"
    echo "      the install step is broken or skipped"
    exit 1
fi

# Sanity: the new binary actually runs.
"$CARGO_BIN" --help >/dev/null

echo "OK: $CARGO_BIN was reinstalled by 'make build' (mtime advanced by $((after_mtime - before_mtime))s)"
