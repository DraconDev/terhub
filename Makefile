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

.PHONY: build test clippy install clean check-lock

# Recommended dev workflow: build + install to ~/.cargo/bin.
build:
	cargo build
	cargo install --path . --offline --locked --force

# Same, but release.
build-release:
	cargo build --release
	cargo install --path . --offline --locked --force --root ~/.cargo

test:
	cargo test --lib --tests

# Strict clippy; the CI gate uses this.
clippy:
	cargo clippy --lib --tests -- -D warnings

# Install the working tree's binary to ~/.cargo/bin without rebuilding.
install:
	cargo install --path . --offline --locked --force

# Lint that the auto-install is actually doing what we say.
check-lock:
	cargo build --locked --offline
	cargo install --path . --offline --locked --force --dry-run

# Verifies that `make build` actually reinstalls the new binary
# to ~/.cargo/bin/terhub. See scripts/check-install.sh.
check-install:
	./scripts/check-install.sh

clean:
	cargo clean
