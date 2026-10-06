//! terhub: terminal-first session hub over tmux.

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

pub mod agent;
pub mod app;
pub mod attend;
pub mod automation;
pub mod config;
pub mod daemon;
pub mod fleet;
pub mod history;
pub mod hooks;
pub mod mcp;
pub mod proc;
pub mod tmux;
pub mod ui;
pub mod web;
pub mod zellij;
