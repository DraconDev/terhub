# terhub build helpers
#
# The plain targets here mirror the cargo commands you'd type by hand
# so the Makefile stays a thin wrapper, not a build system. The `build`
# target is the recommended day-to-day command: it runs `cargo build`
# and then `cargo install --path . --offline --locked --force` so the
# binary on $PATH (`~/.cargo/bin/terhub`) stays in sync with the
# working tree. A stale `~/.cargo/bin/terhub` against a fresh
# `target/debug/terhub` is the most common "is it broken or is the
# binary old?" confusion (see GUIDE.md).
#
# Use plain `cargo build` directly if you don't want to install.

.PHONY: build build-release test clippy install clean check-lock check-install

# Recommended dev workflow: build + install to ${CARGO_HOME:-~/.cargo}/bin.
build:
	cargo build
	cargo install --path . --offline --locked --force --root "$${CARGO_HOME:-$$HOME/.cargo}"

# Same, but release. Honours CARGO_HOME like the other targets:
# a hardcoded ~/.cargo root would install beside a CARGO_HOME on
# PATH and leave the stale binary running (see check-install.sh,
# which resolves the binary as ${CARGO_HOME:-$HOME/.cargo}/bin).
build-release:
	cargo build --release
	cargo install --path . --offline --locked --force --root "$${CARGO_HOME:-$$HOME/.cargo}"

# Same scope as the CI gates (ci.yml): full all-targets runs, so a
# local green means CI green. `--lib --tests` used to skip bins and
# doctests here while CI ran everything.
test:
	cargo test --offline

# Strict clippy; the CI gate uses this exact command.
clippy:
	cargo clippy --offline --all-targets -- -D warnings

# Install the working tree's binary to ${CARGO_HOME:-~/.cargo}/bin
# without rebuilding.
install:
	cargo install --path . --offline --locked --force --root "$${CARGO_HOME:-$$HOME/.cargo}"

# Lint that the auto-install is actually doing what we say.
check-lock:
	cargo build --locked --offline
	cargo install --path . --offline --locked --force --dry-run

# Verifies that `make build` actually reinstalls the new binary
# to ${CARGO_HOME:-~/.cargo}/bin/terhub. See scripts/check-install.sh.
check-install:
	./scripts/check-install.sh

clean:
	cargo clean
