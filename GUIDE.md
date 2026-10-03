# terhub guide

terhub is a terminal-first session hub over tmux: one fullscreen TUI (plus a
plain CLI) to see every session and pane, know which agents need you, jump to
them, and drive them — from scripts, rules, MCP clients, a loopback web
dashboard, or other hosts over SSH. Full Rust, mouse-friendly, non-vim, and
every key rebindable.

If you only want the cheat-sheet, see [README.md](README.md). This guide
explains how the pieces fit and how to set each one up.

## Contents

- [Install](#install)
- [Concepts](#concepts)
- [The TUI](#the-tui)
- [Configuration](#configuration)
- [The daemon and agent awareness](#the-daemon-and-agent-awareness)
- [Hooks: telling terhub what agents do](#hooks-telling-terhub-what-agents-do)
- [Attention: attend, peek, status line](#attention-attend-peek-status-line)
- [History: stats, report, activity, timeline](#history-stats-report-activity-timeline)
- [Automation](#automation)
- [MCP server](#mcp-server)
- [Web dashboard](#web-dashboard)
- [Fleet (multiple hosts)](#fleet-multiple-hosts)
- [Zellij baseline](#zellij-baseline)
- [Command reference](#command-reference)
- [Files and locations](#files-and-locations)
- [Troubleshooting](#troubleshooting)

## Install

Requires tmux 3.x on PATH and Rust 1.88+.

```sh
cargo install --path .
terhub            # opens the TUI
```

`terhub` with no arguments opens the TUI; every subcommand below also works
from scripts. First run creates `~/.config/terhub/config.toml` with defaults.

## Concepts

- **Session / window / pane** are tmux's own. terhub never wraps or renames
  them; pane ids look like `%69`.
- **Agent** is anything running in a pane that behaves like a coding agent
  (claude, codex, gemini, pi, …). terhub tracks each agent pane in one of
  seven states: `working`, `waiting` (needs you), `error` (needs you),
  `idle`, `stuck` (needs you: no CPU for 10+ minutes), `limited` (needs
  you: quota/rate wall, paused until reset — never a crash), `unknown`.
- **Daemon (`terhubd`)** holds the live agent registry. Hook events from
  agents are authoritative; agent identity comes from the process tree and
  states from screen patterns, re-polled every 2 seconds.
- **Ledger** (`~/.local/share/terhub/activity.jsonl`) is the append-only log
  of every agent transition, session open/close, rule firing, and focus
  change the daemon sees. Stats and timelines derive from it.

## The TUI

Run `terhub`. A one-line session bar on top, one huge pane table, one
status bar, no modes except small popups:

- **Top bar: sessions.** `All` (the default) plus one segment per session
  with its attached marker (`●`/`○`) and `(panes·needy!)` counts; long
  lists scroll horizontally (`‹ … ›`). `←`/`→` moves and filters the table
  immediately, `Enter` attaches, `n` new, `r` rename, `x` kill (with
  confirm), `d` detach, `q` quit, `?` shows the current bindings.
- **Table: panes.** Every pane on the server (or the bar session's):
  attention marker (`!` needs you; `*` marks the focused pane, but only
  in windows with several panes, where focus is ambiguous), pane id,
  session (hidden when filtered), window (a `:[N]` suffix appears only
  when the window holds several panes), command, repo dir, agent, state,
  source (`hook` = agent-reported fact, `proc` = agent binary found, `scr` =
  screen guess), age, activity, summary. The summary shows the prompt or
  error line that decided a waiting/error state — a bare prompt borrows
  the nearest text line above it (hook reports and the stuck explanation
  show up here too); working rows stay blank, since spinner text is
  noise, not information.
  `↑`/`↓` moves, `Enter` jumps to the pane, `x` kills it (with confirm),
  `a` attends, `Esc` steps back to the session bar. Long lists scroll;
  the selection is always kept in view.
- **Preview on demand.** Nothing is previewed by default: right-click a
  row or press `p` to open the pane's screen fullscreen; right-click
  again (anywhere) or `Esc`/`p` closes it, left-click/`Enter` jumps to
  the previewed pane.
- **Mouse.** Left-click a row jumps to the pane (press and release must
  land on the same cell, so text drags never jump), right-click toggles
  the fullscreen preview, click a session segment to filter, double-click
  a session to attach.
- **Sorting.** The table sorts by recency (`Act`, the window's last
  activity) by default. Click any header to sort by it, click again to
  flip direction; `o` cycles columns from the keyboard. The `▲`/`▼` mark
  shows the active sort. Unknowns (no daemon record yet) sink to the
  bottom.
- **Filter.** `f` narrows rows by case-insensitive substring across every
  text column (session, window, command, dir, agent, state, summary).
  Typing filters live, `Enter` keeps the query, `Esc` clears it; the
  active query shows in the title bar. It composes with the session
  filter.
  Prompts eat all clicks; the settings popup rows are clickable.
- **Styles.** `t` cycles the six built-in themes and `v` flips table
  density (comfortable shows the summary column, compact drops it and
  tightens columns) — both session-only try-outs. `s` opens the settings
  popup (style picker, density, notification and highlight toggles) which
  applies and saves to the config file.
- **Popups.** `n`/`r` open a text prompt with a real cursor (`←`/`→`,
  `Home`/`End`, `Backspace`/`Delete`, `Enter` confirms, `Esc` cancels).
  Kills ask for confirmation (`Enter`/`y` yes, `Esc`/`n` no).
- **`Ctrl+C` always quits.** `Esc` quits from the session bar.
- **Agent columns.** With the daemon running, rows show agent/state/age
  and the title bar counts how many need you.

Everything except arrows/mouse/Tab/Esc is rebindable (see
[Configuration](#configuration)); the `?` help always shows your current
bindings.

## Configuration

`~/.config/terhub/config.toml` (see `terhub config --show`, `--path`,
`--init`):

```toml
[ui]
theme = "dark"       # dark | light | dracula | nord | gruvbox | solarized
density = "comfortable"  # comfortable | compact
highlight_attached = true

[keys]
new = "n"
kill = "x"
rename = "r"
attach = "enter"
detach = "d"
quit = "q"
help = "?"
refresh = "f5"
attend = "a"
preview = "p"
theme = "t"
density = "v"
settings = "s"
sort = "o"
filter = "f"

[notifications]
enabled = true
```

Key names: single characters, `enter`, `esc`, `tab`, `f5`, … Unknown keys
fall back to defaults. Notifications gate the desktop popups on agent state
changes. `terhub config --check` validates the file and names every problem
(unknown theme/density/keys, TOML syntax errors); the TUI also surfaces the
first problem in its status bar instead of silently running on defaults.

## The daemon and agent awareness

```sh
terhub daemon start|stop|status
terhub status [--json]
```

`terhubd` keeps the registry that powers `status`, `attend`, badges, rules,
and MCP. One daemon per runtime dir (a second start refuses with "already
listening"). Per pane it merges three signals:

1. **Hook events** (`terhub hook …`, see below) — authoritative while fresh.
   A hook record with no new event for `stale_secs` (default 30 minutes)
   lapses back to automatic classification, keeping its clock if the state
   didn't change (no renotify for a pane that sat still) but dropping its
   stale summary.
2. **Process tree** — the pane's descendants are matched against the agent
   catalog (claude, codex, gemini, pi, muse, aider, crush, opencode, …),
   seeing
   through wrappers like shells and `systemd-inhibit`. Session harnesses
   (muxa, tmux) are transparent: detection looks *through* them for the
   real agent. This decides
   *who* the agent is; a live agent with an unreadable screen reads as
   `working`. Its subtree CPU is tracked: `working` with no CPU for
   `stuck_secs` (default 10 minutes) becomes `stuck` (needs you), clearing
   the moment it computes again (`stuck_secs = 0` disables stuck
   detection). Fresh hooks are never second-guessed this way.
3. **Screen patterns** — built-in manifests for claude/codex/gemini match
   the pane's command and recent screen text. The freshest matching line
   decides, so current output overrides stale scrollback; `waiting`/`error`
   on screen win over the process state. Harness dashboards (muxa,
   terhub's own TUI) skip screen classification — they name agents
   without running them. A record with no supporting evidence for 30
   minutes is dropped rather than shown. Idle shells nobody claimed are
   skipped, so plain terminals stay invisible.

The table's `Src` column tells you which signal each row stands on.

Custom agents go in `~/.config/terhub/manifests/*.toml`:

```toml
agent = "myagent"
commands = ["myagent", "ma"]
working = ["Thinking", "⠋|⠙|⠹"]
waiting = ["confirm", "\\[y/n\\]"]
error = ["\\berror\\b", "traceback"]
```

Patterns are case-insensitive regexes; within one line the most specific
signal wins (limited beats error beats waiting beats working), and the
freshest matching line wins across lines. A `limited` list holds
quota/rate-limit language so a quota wall never reads as a crash. Without
a command match, patterns still yield a
state, but the agent stays unnamed unless the screen mentions it: a bare
approve prompt plus a live agent process reads as that agent waiting,
while the same prompt with no process behind it claims nobody. A manifest
with a broken regex is skipped with a warning instead of killing the
daemon.

## Hooks: telling terhub what agents do

Hooks are fire-and-forget, made for agent lifecycle scripts:

```sh
terhub hook --agent claude --pane %1 --state waiting --summary "Approve plan?"
```

- `--state` accepts `working|waiting|error|idle|stuck|limited|unknown`
  plus aliases: `work/busy/running/start`, `wait/input/attention/blocked/
  needs-input/stop`, `failed/fail`, `done/complete/completed`, `hung`,
  `limit/quota/capped`.
  `start`/`stop` match the lifecycle states that spawning harnesses emit.
- `--pane` is the pane id (`%N`); inside tmux, `$TMUX_PANE` has it.
- `--summary` is free text shown in peek/status and matchable by rules.
- Unknown states are rejected; if the daemon is down the hook fails loudly
  (exit 1) instead of vanishing.

State changes from hooks are logged to the ledger and fire desktop
notifications (unless disabled).

## Attention: attend, peek, status line

```sh
terhub attend [--cycle] [--list]
terhub peek [--plain]
terhub status-line [--pane %N]
```

- **`attend`** jumps to the longest-waiting agent (`--list` only lists,
  `--cycle` advances through everyone needy in turn). In the TUI, `a`
  attends from either list. Outside tmux without a terminal it refuses
  cleanly instead of scrambling server selection.
- **`peek`** shows this window's panes with agent states as a tmux popup
  (digits jump to a pane); `--plain` prints the same report as text for
  scripts and status bars.
- **`status-line`** prints one line for `status-right`, e.g.
  `set -g status-right '#(terhub status-line)'`. Always exits 0 (empty
  when the daemon is down).

## History: stats, report, activity, timeline

```sh
terhub stats [--since today] [--group-by day|agent|session]
terhub report [--since week]
terhub activity [--since 24h] [--kind agent|session|rule|human|all] [--json]
terhub timeline [--since today] [--session S] [--agent A]
```

`--since` accepts `today|yesterday|week|month|all`, durations like `24h` /
`7d`, or `YYYY-MM-DD` (impossible dates such as 02-31 are rejected, not
normalized). Day boundaries are UTC. `timeline` opens at the newest entries;
`q`/`Esc` quits, arrows/`PgUp`/`PgDn` scroll.

Entry kinds: `agent` (state transitions), `session` (open/close), `rule`
(automation firings), `human` (your focus moving between panes).

## Automation

`~/.config/terhub/automation.toml` holds rules that act on agent state
(`terhub automation list` validates and shows recent fire counts;
`terhub automation test <name>` dry-runs one rule against live state):

```toml
[[rule]]
name = "resume-after-limit"
enabled = false            # arm deliberately
on = "waiting"             # state (aliases accepted)
match_summary = "limit"    # optional substring filters
agent = "claude"           # optional substring filter
after_secs = 300           # entry delay: hold the state this long first
cooldown_secs = 3600       # per-pane quiet period after a firing
hourly_cap = 3             # max firings per rule per hour (0 = invalid)
action = "send-keys"       # or "shell"
text = "continue"          # send-keys payload (submitted with Enter)
# command = "notify-me"    # shell payload instead; TERHUB_* env set
```

Guardrails are mandatory: delay, per-pane cooldown, and hourly cap (counted
inside the pass, so one tick can never overfire). When several panes match
and slots are scarce, the longest-waiting panes win. Shell actions time out
after 60 seconds so a hanging command can't stall the daemon. Every firing
(and failure) is logged to the ledger as a `rule` entry. Rules with problems
(bad state/action, missing payload, zero cap) are listed but never fire.

## MCP server

`terhub mcp` speaks MCP over stdio (Content-Length framing) so orchestrating
agents can observe and drive the hub. Tools:

- `terhub_status` — agent states: which panes run agents, who waits/errors.
- `terhub_capture` — recent screen content of a pane.
- `terhub_send` — type text into a pane (submits with Enter by default).
- `terhub_wait` — block until a pane (or any agent) reaches a state; times
  out with an error instead of hanging forever.
- `terhub_attend` — the longest-waiting agent needing attention.

Point any MCP-capable client at the command; no config needed beyond a
running daemon for state tools (`capture`/`send` talk to tmux directly).

## Web dashboard

```sh
terhub serve --port 8080   # open http://127.0.0.1:8080/
terhub serve --port 0      # ephemeral port, prints the URL
```

Loopback-only by construction (it binds `127.0.0.1`, never `0.0.0.0`): a
live agent table, per-pane capture viewer, prompt + interrupt actions,
today's stats, and the timeline, refreshing every couple of seconds.

JSON API for scripts: `GET /api/status`, `GET /api/stats?since=&group=`,
`GET /api/timeline?since=&session=&agent=&limit=` (newest-first windowing),
`GET /api/capture?pane=&lines=`, `POST /api/send {pane, text, submit?}`,
`POST /api/interrupt {pane}`. Status/stats/timeline answer 503 when the
daemon is down rather than serving stale data.

## Fleet (multiple hosts)

Run terhub + daemon on each host, register them locally:

```sh
terhub host add <name> <ssh-target> [--mode observe|control]
terhub host list|doctor <name>|remove <name>
terhub fleet status [--json]
terhub fleet send <host> <pane> <text>
```

`fleet status` queries all hosts concurrently over `ssh` and reports each
host's agents (or the ssh error) in config order. `fleet send` only works
on `control`-mode hosts; `observe`-mode hosts refuse. `host doctor` checks
the remote for ssh reachability, `terhub` on PATH, tmux, and a running
daemon. Hosts live in `~/.config/terhub/hosts.toml`.

## Zellij baseline

```sh
terhub zellij list|attach <name>
```

Lists and attaches to zellij sessions when `zellij` is on PATH. This is a
listing baseline only — pane observation, agent awareness, and automation
are tmux-backed.

## Command reference

| Command | What it does |
|---|---|
| `terhub` | Open the TUI hub |
| `list [--plain]` | List sessions |
| `new <name> [--dir DIR] [--window NAME]` | Create a session |
| `attach <name>` | Attach (needs a terminal) |
| `kill <name>` | Kill a session |
| `panes <session>` | List a session's panes |
| `capture <pane> [--lines N]` | Print pane content |
| `focus <pane>` | Jump to a pane |
| `kill-pane <pane>` | Kill one pane |
| `swap-pane <a> <b>` | Swap two panes |
| `send <pane> <text> [--no-submit]` | Type into a pane |
| `status [--json]` | Agent states from the daemon |
| `hook --agent A --pane P --state S [--summary T]` | Push a hook event |
| `daemon start\|stop\|status` | Manage `terhubd` |
| `attend [--cycle] [--list]` | Jump to longest-waiting agent |
| `peek [--plain]` | This window's panes + states |
| `status-line [--pane P]` | One line for tmux status-right |
| `stats [--since S] [--group-by G]` | Time-in-state stats |
| `report [--since S]` | All breakdowns at once |
| `activity [--since S] [--kind K] [--json]` | Raw ledger entries |
| `timeline [--since S] [--session S] [--agent A]` | Scrollable timeline |
| `automation list\|test <name>` | Rules: validate / dry-run |
| `mcp` | MCP stdio server |
| `serve --port P` | Loopback web dashboard |
| `host add\|list\|doctor\|remove` | Fleet host registry |
| `fleet status [--json]` | Agents across hosts |
| `fleet send <host> <pane> <text>` | Drive a control-mode host |
| `zellij list\|attach` | Zellij baseline |
| `config --show\|--path\|--init\|--check` | Config file |

## Files and locations

| Path | What |
|---|---|
| `~/.config/terhub/config.toml` | Keys, theme, notifications |
| `~/.config/terhub/automation.toml` | Automation rules |
| `~/.config/terhub/hosts.toml` | Fleet hosts |
| `~/.config/terhub/manifests/*.toml` | Extra agent screen-patterns |
| `~/.local/share/terhub/activity.jsonl` | Activity ledger (90-day prune) |
| `$XDG_RUNTIME_DIR/terhub/` | Socket, pid, registry cache |

`TERHUB_RUNTIME_DIR` / `TERHUB_DATA_DIR` / `XDG_*_HOME` override locations
(empty values are ignored). The registry cache is rebuilt from tmux on every
daemon start; only the ledger is long-term memory.

## Troubleshooting

- **"daemon not running"**: `terhub daemon start`. Only one daemon per
  runtime dir; a second start says "already listening". A failed start
  prints the tail of `$XDG_RUNTIME_DIR/terhub/terhubd.log` with the reason.
- **Pane shows no agent**: idle shells are skipped until a hook, a known
  agent process, or a screen pattern claims them. Add a manifest or emit
  hooks from the agent; unknown agent binaries are a feature request.
- **Agent stuck in a state**: hooks go stale after `stale_secs` of silence
  and fall back to screen classification; check the agent still emits hooks.
- **Rule never fires**: `terhub automation list` shows validation problems
  and recent counts; `automation test <name>` dry-runs against live state.
  Remember delay + cooldown + cap all apply.
- **"cannot attach: no terminal"**: `attach`/`focus`/`attend` outside tmux
  need a real terminal — they refuse instead of moving your sessions.
- **`serve` unreachable**: it only binds loopback by design; open it on the
  same machine (or SSH-forward the port).
- **Times look off by hours**: ledger day boundaries are UTC; local display
  may differ by your timezone offset.

