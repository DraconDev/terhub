//! terhub — a terminal command center.
//!
//! A fullscreen TUI plus a plain CLI that gathers every session, every
//! pane, and every agent from every place you work — local tmux, local
//! zellij, GNU screen, SSH hosts — into one screen, names which ones
//! need you, and lets you jump to them, mark them, and drive them.
//! Think of it as the overview that the terminal never had.

/// FNV-1a 64-bit: deterministic hash for change detection and stable
/// name coloring. SipHash is randomly keyed per process, which would
/// restart quiet clocks and reshuffle colors on every launch.
pub fn fnv1a(s: &str) -> u64 {
    let mut h: u64 = 0xcbf29ce484222325;
    for b in s.as_bytes() {
        h ^= *b as u64;
        h = h.wrapping_mul(0x100000001b3);
    }
    h
}

/// This build's identity: a fingerprint of the sources it was built
/// from (see `build.rs`).
///
/// Every binary in the package carries the same id, so the client and
/// the daemon can report whether they are one build. Two processes
/// that disagree here are talking past each other, and the protocol
/// version is the part that actually matters — this is the provenance
/// that says *which* build.
pub const BUILD_ID: &str = env!("TERHUB_BUILD_ID");

/// The package version, for display next to the build id.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

pub mod agent;
pub mod app;
pub mod attend;
pub mod automation;
pub mod capture;
pub mod config;
pub mod daemon;
pub mod fleet;
pub mod history;
pub mod hooks;
pub mod mcp;
pub mod native;
pub mod proc;
pub mod screen;
pub mod tmux;
pub mod tray;
pub mod ui;
pub mod web;
pub mod zellij;
