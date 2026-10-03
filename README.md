# terhub

Terminal-first session hub over tmux. Mouse-friendly, customizable, non-vim. Full Rust (ratatui).

## Run

```
cargo build
cargo run -- list
cargo run          # TUI hub
```

Install: `cargo install --path .`, then `terhub`.

## Use (TUI)

- One-line session bar + one huge pane table (state, source, agent, dir, age, activity, summary). Recency sort by default, click any header to sort. Waiting/error rows show the prompt or error line in Summary.
- Left-click jumps, right-click toggles the fullscreen preview, double-click a session attaches.
- `n` new, `r` rename, `x` kill, `d` detach, `a` attend, `p` preview, `t` theme, `v` density, `s` settings, `o` sort, `f` filter, `space` mark, `u` unmark, `q` quit, `?` help. Up/Down move, Left/Right filter, Tab switches lists.
- Keys + theme + density in `~/.config/terhub/config.toml` (auto-created). Themes: dark, light, dracula, nord, gruvbox, solarized.

## CLI

```
terhub list [--plain]
terhub new <name> [--dir DIR] [--window NAME]
terhub attach <name>
terhub kill <name>
terhub panes <session>
terhub capture <pane> [--lines N]
terhub focus <pane>
terhub kill-pane <pane>
terhub swap-pane <a> <b>
terhub config --show|--path|--init|--check
```

TUI: Tab switches between the session bar (filters the table) and the
pane table; Enter on a pane jumps to it, `x` kills it (with confirm),
`p`/right-click opens the preview fullscreen. Agent columns appear when the daemon runs.

## Daemon (agent awareness)

```
terhub daemon start|stop|status
terhub status [--json]     # which agents need you (waiting/error/stuck/limited)
terhub hook --agent claude --pane %1 --state waiting --summary "..."
```

`terhubd` watches every pane: hook events are authoritative, and
hook-less panes are classified from screen patterns (built in for
claude/codex/gemini, extend via `~/.config/terhub/manifests/*.toml`).

## Attention

```
terhub attend [--cycle] [--list]   # jump to longest-waiting agent
terhub peek [--plain]              # this window's panes as an overlay
terhub status-line [--pane %N]     # for tmux status-right
```

State changes fire desktop notifications (`notify-send`; disable with
`[notifications] enabled = false`). In the TUI, `a` attends.

## History

The daemon logs every agent transition and session open/close to
`~/.local/share/terhub/activity.jsonl` (days are UTC).

```
terhub stats [--since today] [--group-by day|agent|session]
terhub report [--since week]
terhub activity [--since 24h] [--kind agent|session|rule|human|all] [--json]
terhub timeline [--since today] [--session S] [--agent A]
```

## Automation + MCP

```
terhub automation list          # rules in ~/.config/terhub/automation.toml
terhub automation test <name>   # dry-run against live state
terhub mcp                      # MCP stdio server (status/capture/send/wait/attend)
```

Rules fire on agent state with mandatory delay, per-pane cooldown, and
hourly cap; every firing is logged to the ledger.

## Web dashboard

```
terhub serve --port 8080   # then open http://127.0.0.1:8080/ in a browser
```

Loopback-only (never binds anything else): live agent table, per-pane
capture, prompt + interrupt actions, today's stats, timeline. `--port 0`
picks an ephemeral port and prints the URL.

## Fleet + zellij

```
terhub host add <name> <ssh-target> [--mode observe|control]
terhub host list|doctor <name>|remove <name>
terhub fleet status [--json]       # agents across hosts over ssh stdio
terhub fleet send <host> <pane> <text>   # control-mode hosts only
terhub send <pane> <text> [--no-submit]
terhub zellij list|attach <name>   # baseline backend (no pane observation)
```

## Dev

```
cargo test
cargo clippy --all-targets -- -D warnings
cargo fmt --check
```

Requires tmux 3.x on PATH. Rust 1.88+.
