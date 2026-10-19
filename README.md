# terhub — your terminal command center

[![Topics: tmux, zellij, screen, tui, ratatui, rust, mcp-server, ssh, agent-monitor, command-center](https://img.shields.io/github/topics/DraconDev/terhub?style=flat-square)](https://github.com/DraconDev/terhub)

> **terhub** is a fullscreen TUI (plus a plain CLI) that gathers every
> session, every pane, and every agent from every place you work
> (local tmux/zellij/screen, SSH hosts, agent processes) into one
> screen, names which ones need you, and lets you jump to them, mark
> them, and drive them. It's the overview that the terminal never had.

If you've ever lost a pane in a stack of `tmux ls` outputs, missed an
agent waiting for input because you were in the wrong window, or wanted
to see at a glance which of fifteen sessions actually need you —
terhub is the screen that fixes that. It works on top of tmux, zellij,
and screen locally, and reaches across SSH to other hosts.

![terhub TUI: every pane, two red rows that need you, filter chip, footer of keys](https://github.com/DraconDev/terhub/raw/main/assets/screenshot.png)

## Backend parity

Every backend is the same TUI; what differs is which CLI verbs terhub has
implemented for it. A `-` in the table means the verb is not wired yet,
so a row in that backend will be visible but its `p`/`x`/`r` keys won't
do anything (you can still attach and read).

| Capability           | tmux | zellij | screen | SSH fleet | native tabs |
|----------------------|:----:|:------:|:------:|:---------:|:-----------:|
| List sessions        |  ✓   |   ✓    |   ✓    |     ✓     |      ✓      |
| Attach               |  ✓   |   ✓    |   ✓    |     ✓     |      ✓      |
| List panes           |  ✓   |   –    |   –    |     ✓     |      –      |
| Capture last lines   |  ✓   |   –    |   –    |     ✓     |      –      |
| Kill / swap / focus  |  ✓   |   –    |   –    |     ✓     |      –      |
| Agent hook events    |  ✓   |   –    |   –    |     –     |      –      |
| Activity sparkline   |  ✓   |   –    |   –    |     ✓     |      –      |
| Freezing / thawing   |  ✓   |   –    |   –    |     ✓     |      –      |

Native tabs are Konsole, kitty, and WezTerm — the TUI lists them so you
can `Enter` into them; it does not drive their panes. SSH fleet goes
through the `terhub` binary on the remote, so anything the remote's
backend supports is supported there too.

## Install

Requires tmux 3.x on PATH and Rust 1.89+.

```sh
cargo install --path .
terhub init       # config + agent hooks + daemon, one shot
terhub            # opens the TUI
```

First run creates `~/.config/terhub/config.toml` with defaults.
Full manual: [GUIDE.md](GUIDE.md).

Project status: [AUDIT.md](AUDIT.md) (the 2026-10-19 audit — 36 findings) and
[TASKS.md](TASKS.md) (the prioritised improvement backlog).

## The TUI

One-line session bar on top, one huge pane table, one status bar — no modes
except small popups:

- **Session bar.** `All` plus one segment per session, with attached marker
  (`●`/`○`), pane counts, and a worst-state dot (red = something needs you).
  `←`/`→` filters the table immediately, `Enter` attaches, `Alt+1..8` jumps
  to tabs.
- **Pane table.** Every pane: attention mark (`!` needs you, `+` marked,
  `*` focused, `✓` already seen), pane id, session, window, command, repo
  dir, agent, state, source (`hook` = agent-reported fact, `proc` = binary
  found, `scr` = screen guess), time in state, last turn, idle age, and a
  summary carrying the prompt or error line that decided the state.
- **States that matter are red.** Waiting, error, stuck, and quota-limited
  rows shout; working/idle are quiet. The filter chip (`◼`) names every
  filter hiding rows so one is never silently active.
- **Preview on demand.** Right-click or `p`: fullscreen scrollable reader
  over the pane's last 500 lines with its own ANSI colors, live CPU, and —
  in git repos — branch plus diff shortstat.
- **Filters.** `f` searches (or just start typing); `!` excludes. The
  standing exclusion from `[ui] filter` is always on. Name multi-token
  filters in settings (`s` → Filters) and cycle them with `ctrl+f`.
- **Marks & bulk.** `Space` marks rows file-manager style; `x`/`a` act on
  the whole marked set under one confirmation.
- **Record/share mode.** Settings (`s` → Toggles → *blur columns*) fills
  the identity-revealing columns — Session, Window, Cmd, Dir, Agent,
  Handle, Summary, plus the session-bar tab names — with a solid `█` of
  the same width, so a recording or shared screenshot shows which agents
  need you and how many are idle without revealing the repos, commands,
  or prompts. State, source, counts, and the marks stay readable; no
  keybind, so it's obvious when it's armed.
- **Sorting.** Recency sort by default; click any header (or `o`) to sort.

Keys (all rebindable in `[keys]`):

`n` new · `r` rename · `x` kill · `d` detach · `a` attend · `p` preview ·
`t` theme · `v` density · `s` settings · `o` sort · `f` filter ·
`ctrl+f` saved filters · `space` mark · `u` unmark · `h` hide quiet shells ·
`w` spawn agent · `e` restart · `z` freeze/thaw · `m` hosts · `b` bulk ·
`F5` refresh · `q` quit · `?` help (twice for the legend)

## Daemon + hooks

`terhubd` holds the live agent registry. Hook events from agents are
authoritative; identity comes from the process tree and states from screen
patterns, re-polled every 2 seconds. Every transition lands in the ledger
(`~/.local/share/terhub/activity.jsonl`), which powers stats, reports,
timelines, and per-pane history.

Agent states: `working`, `waiting` (needs you), `error` (needs you),
`idle`, `done` (finished — never needs you), `stuck` (needs you: no CPU
for 10+ minutes), `limited` (needs you: quota/rate wall, paused until
reset — never a crash), `unknown`.

## Tray icon (KDE/Plasma)

```sh
terhub tray
```

A StatusNotifierItem until killed: red when something needs you, teal when
agents are done, gray + strikethrough when the daemon is down. Counts live
in the hover tooltip; clicking focuses the open hub or starts one.

Nothing supervises `terhubd`, so it can die (terminal SIGHUP, the OOM
killer). The hub notices on the next refresh and starts it again — the
`daemon down` banner is diagnostic, not a tombstone.

## CLI

```
terhub list|peek            # panes as text / overlay
terhub status-line          # one line for tmux status-right
terhub stats|report|activity|recap|timeline
terhub automation list|test # rules with delay + cooldown + cap
terhub mcp                  # MCP stdio server
terhub serve --port 8080    # loopback web dashboard
terhub host|fleet ...       # agents across hosts over SSH
terhub send <pane> <text>   # drive a pane from scripts
```

See `terhub --help` and [GUIDE.md](GUIDE.md) for the rest.

## Dev

```
cargo test --offline
cargo clippy --offline --all-targets -- -D warnings
cargo fmt --check
```
