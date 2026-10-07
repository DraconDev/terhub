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
terhub init       # config + agent hooks + daemon, one shot
terhub            # opens the TUI
```

`terhub` with no arguments opens the TUI; every subcommand below also works
from scripts. First run creates `~/.config/terhub/config.toml` with defaults.
`init --dry-run` prints the three steps without running them.

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
  with its attached marker (`●`/`○`) and `(panes·needy!)` counts, plus a
  worst-state dot (red = something needs you, green = working away;
  idle-only tabs stay dotless); long lists scroll horizontally (`‹ … ›`).
  `←`/`→` moves and filters the table immediately, `Enter` attaches,
  `n` new, `w` spawn agent, `r` rename, `x` kill (with confirm), `d`
  detach, `q` quit, `?` shows the current bindings; `Alt+1..8` jumps
  straight to tabs (`All` + first 7 sessions). `mru_tabs` (settings
  toggle) orders tabs by recent activity instead of tmux order.
- **Table: panes.** Every pane on the server (or the bar session's):
  attention marker (`!` needs you; `*` marks the focused pane, but only
  in windows with several panes, where focus is ambiguous), pane id,
  session (hidden when filtered), window (a `:[N]` suffix appears only
  when the window holds several panes), command, repo dir, agent, state,
  source (`hook` = agent-reported fact, `proc` = agent binary found, `scr` =
  screen guess), age, last-turn duration, activity, summary. The summary shows the prompt or
  error line that decided a waiting/error state — a bare prompt borrows
  the nearest text line above it (hook reports and the stuck explanation
  show up here too); working rows stay blank, since spinner text is
  noise, not information. Turn is the last completed working turn (a turn
  ends when working moves to waiting/error/limited/idle; stuck never ends
  one — the same turn is grinding). `Cmd` unwraps launcher wrappers
  (`systemd-inhibit`, `sudo`, …) to the payload program; an agent with an
  editor (`micro`, `vim`, …) in the foreground reads waiting — it
  summoned `$EDITOR` and sits on keystrokes. The TUI's own row wears the
  attached accent on its id, and a vanished cwd reads struck-through.
  `↑`/`↓` moves, `Enter` jumps to the pane, `x` kills it (with confirm),
  `a` attends, `Esc` steps back to the session bar. `w` spawns an agent
  window with an opening prompt (profile picker on `Tab`), `e` restarts
  the pane's command (with confirm), `z` freezes/thaws the pane's process
  tree. `Space` toggles a `+`
  mark on the row and steps down (file-manager style), `u` clears all
  marks; with marks set, `x` kills the whole marked set under one
  confirmation and `a` attends the longest-waiting marked pane. Marks
  survive sort/filter and show as a count in the title bar. Long lists scroll;
  the selection is always kept in view.
- **Preview on demand.** Nothing is previewed by default: right-click a
  row or press `p` to open a scrollable fullscreen reader over the pane's
  last 500 lines, with the pane's own ANSI colors; right-click again
  (anywhere) or `Esc`/`p` closes it, left-click/`Enter` jumps to the
  previewed pane. Wheel, `PgUp`/`PgDn`, `Home`/`End` scroll the reader
  (position shows bottom-right, `40/500`); `↑`/`↓` still moves the
  selection and the reader follows the new pane from the top. Background
  fills reach the pane edge (tmux trims the painted EOL cells; the preview
  rebuilds them from the leftover style codes). The title carries the
  pane's live CPU percent (over 100% is real — agent subtrees span cores)
  and, when the pane sits in a git repo, the branch plus diff shortstat
  (`git main +45/-12 · 2 changed · 1 new`, or `clean`).
- **Mouse.** Left-click a row jumps to the pane (press and release must
  land on the same cell, so text drags never jump), right-click toggles
  the fullscreen preview, click a session segment to filter, ctrl+click a
  session to attach (double-click is gone: the first click already
  selects, so it could never fire). The row under the cursor highlights
  (bold). There is no mouse-back button (crossterm only delivers
  left/right/middle) — `Esc` steps back instead: preview, then filter,
  then the session bar, then quit.
- **Sorting.** The table sorts by the `Idle` column
  (most-recently-active first) by default. Click any header to sort by
  it, click again to flip direction; `o` cycles columns from the
  keyboard. The `▲`/`▼` mark shows the active sort. Unknowns (no daemon
  record yet) sink to the bottom.
- **Filter.** `f` narrows rows by case-insensitive substring across every
  text column (session, window, command, dir, agent, state, summary).
  Typing filters live, `Enter` keeps the query, `Esc` clears it; the
  active query shows in the title bar. It composes with the session
  filter. `h` hides unclaimed panes quiet over 10 minutes (one key,
  reversible, hidden count in the title) — the fast way to drop idle
  shells from a dashboard view.
  Prompts eat all clicks; the settings popup rows are clickable.
- **Refresh.** `F5` re-reads tmux and re-fetches native terminal tabs
  (Konsole/kitty/WezTerm show as `konsole:…` rows under `All` only; see
  [Native terminal tabs](#native-terminal-tabs)).
- **Bulk prompt.** `b` sends one message to every marked pane — or, with
  no marks, to every agent waiting on you. `Tab` cycles `[[template]]`
  snippets into the text; `Enter` sends with submit (these are prompts,
  not drafts). Remotes and your own pane are never targets; failures are
  counted in the footer.
- **Styles.** `t` cycles the ten built-in themes and `v` flips table
  density (comfortable shows the summary column, compact drops it and
  tightens columns) — both session-only try-outs. `s` opens the settings
  popup (style picker, density, notification, mru-tabs and highlight
  toggles) which applies and saves to the config file.
- **Popups.** `n`/`r`/`w` open a text prompt with a real cursor
  (`←`/`→`, `Home`/`End`, `Backspace`/`Delete`, `Enter` confirms, `Esc`
  cancels); the spawn prompt also cycles agent profiles on `Tab`.
  Kills and restarts ask for confirmation (`Enter`/`y` yes, `Esc`/`n` no).
- **`Ctrl+C` always quits.** `Esc` quits from the session bar.
- **Stays open.** Jumps, attends, and attaches only move your view to the
  target pane or session — the table keeps running behind it, so keep it
  in its own pane and flip back to a live table. Only `q`, `Esc` (from the
  session bar), and `Ctrl+C` quit. Outside tmux, an attach still takes over
  the terminal (it has to become a tmux client).
- **Agent columns.** With the daemon running, rows show agent/state/for
  (time in this state) and the title bar counts how many need you. Panes
  with no agent show no state (`—`): a shell was never expected to work
  on its own, so only agents get working/waiting/idle. The `Idle` column shows time since the
  screen meaningfully changed — spinner frames, footer-tip carousels, and
  counter ticks (elapsed times, token rates) don't count, so a spinner
  can't look busy forever; a changed summary line still resets it.
  Unclaimed panes fall back to tmux's window-activity age. Agent cells
  share one color per agent name (see `[style]`), and `Idle` tints when
  a row is `working` but quiet past `stuck_secs` — the stuck smell —
  or `idle` but quiet past `idle_flag_secs` (default 6h, `0` disables),
  without relabeling the state.

Everything except arrows/mouse/Tab/Esc/Alt+digits is rebindable (see
[Configuration](#configuration)); the `?` help always shows your current
bindings (press `?` again for the legend: marks, `Src`, `For`/`Turn`/`Idle`).
Footer confirmations fade after a few seconds; the title names a dead
daemon instead of silently emptying the agent columns.

## Configuration

`~/.config/terhub/config.toml` (see `terhub config --show`, `--path`,
`--init`):

```toml
[ui]
theme = "dark"       # dark | light | dracula | nord | gruvbox | solarized | tokyonight | catppuccin | onedark | monokai
density = "comfortable"  # comfortable | compact
highlight_attached = true
mru_tabs = false     # order session tabs by recent activity

[keys]
new = "n"
spawn = "w"
restart = "e"
pause = "z"
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
mark = "space"
unmark = "u"
hide_shells = "h"
hosts = "m"
bulk = "b"

[notifications]
enabled = true

[daemon]
poll_secs = 2        # re-poll cadence (restart daemon to apply)
stuck_secs = 600     # still-CPU → stuck (0 disables)
stale_secs = 1800    # evidence staleness for carry + hook lapse
quiet_idle_secs = 300 # frozen screen → idle (0 disables)

[style]
color_agents = true  # hash-color Agent cells so one agent reads as a group
agent_palette = []   # style specs for the hash; empty = theme default
idle_warn = true     # tint Idle when working + quiet past stuck_secs
idle_warn_style = "bold yellow"
idle_flag_secs = 21600  # also tint Idle when idle + quiet past this (0 disables)

[style.agents]        # per-agent overrides (style specs)
pi = "cyan"
```

Custom pane commands run a shell template against the selected row:
`[[command]]` with `name`, `key` (must not collide with a built-in), and
`run`. `{pane} {session} {window} {dir} {cmd} {agent} {state}` expand per
row (`{{` escapes a brace); output shows in a scrollable popup (10s cap,
stderr kept and marked). The same `TERHUB_*` environment as automation
rules is set. Example: `name = "git status"`, `key = "G"`,
`run = "git -C {dir} status -sb"`.

Spawn profiles name the agents `w` offers: `[[profile]]` with `name`,
`program`, and optional `dir` (start directory). Without any
`[[profile]]` the builtins apply (`claude`, `codex`, `pi`, `gemini`,
`muse`); user entries replace them wholesale. Example:
`name = "pi-fast"`, `program = "pi --model fast"`.

Message templates are reusable prompt snippets: `[[template]]` with
`name` and `text`. Without any `[[template]]` the builtins apply
(`continue`, `status`, `review`); user entries replace them wholesale.
Templates feed the bulk prompt (`Tab` cycles them into the text) and
`send --template NAME` for scripts.

Key names: single characters, `enter`, `esc`, `tab`, `f5`, … Unknown keys
fall back to defaults. Notifications gate the desktop popups on agent state
changes. A style spec is optional modifiers plus a color (`"bold yellow"`,
`"red"`, `"#ff8800"`; modifiers `bold dim italic underlined reverse crossed
blink`, colors `black red green yellow blue magenta cyan white gray` with
`bright-`/`light-` variants, `#rgb`/`#rrggbb`, or `none` for plain). Unknown
spec tokens degrade to plainer text at render time. `terhub config --check`
validates the file and names every problem
(unknown theme/density/keys, bad `[[command]]`/`[[profile]]` entries,
bad style specs, TOML syntax errors); the TUI also surfaces the
first problem in its status bar instead of silently running on defaults.

## The daemon and agent awareness

```sh
terhub daemon start|stop|status
terhub status [--json]
```

`terhubd` keeps the registry that powers `status`, `attend`, badges, rules,
and MCP. One daemon per runtime dir (a second `daemon start` prints
`already running` and exits 0). Per pane it merges three signals:

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
   detection). A live agent whose **screen has not changed** for
   `quiet_idle_secs` (default 5 minutes) becomes `idle` — a finished turn
   prints whatever it prints, and most print nothing that matches an idle
   pattern, so without this they read `working` forever (their REPL keeps
   burning CPU, so stuck detection never trips either). The screen moving
   again flips them straight back to `working`; `0` disables it. Stuck is
   still CPU-based, so it outranks the quiet flip and an `idle` row can
   still escalate to `stuck`. Fresh hooks are never second-guessed this way.
3. **Screen patterns** — built-in manifests for claude/codex/gemini/muse
   match the pane's command and recent screen text. The freshest matching
   line decides, so current output overrides stale scrollback; `waiting`/
   `error` on screen win over the process state. Harness dashboards (muxa,
   terhub's own TUI) skip screen classification — they name agents
   without running them. A record with no supporting evidence for 30
   minutes is dropped rather than shown. Idle shells nobody claimed stay
   unclaimed, so plain terminals show no state instead of guessing one.

The table's `Src` column tells you which signal each row stands on.

Every agent record also carries a **handle**: a stable local name minted
once (`@pi-wez`, `-2`/`-3`… on collision), surviving pane churn inside
the session (pane ids don't). Handles persist across daemon restarts,
follow the agent when it changes (a new agent in the pane mints fresh),
and show in the table's `Handle` column (sortable), `status`, and
`status --json`. Any command taking a pane id also takes a handle:
`focus @pi-wez`, `send @pi-wez "go"`, `capture @pi-2`, `recap --pane
@pi-wez`. Native tabs mint theirs against the same namespace
(`@pi-konsole`); fleet rows carry none — handles are local-only.

Custom agents go in `~/.config/terhub/manifests/*.toml`:

```toml
agent = "myagent"
commands = ["myagent", "ma"]
working = ["Thinking", "⠋|⠙|⠹"]
waiting = ["confirm", "\\[y/n\\]"]
error = ["\\berror\\b", "traceback"]
limited = ["quota", "rate limit", "\\b429\\b"]
idle = ["task complete", "all done"]
idle_fallback = ["enter send"]
waiting_unless = ["esc to interrupt"]
```

Patterns are case-insensitive regexes; within one line the most specific
signal wins (limited beats error beats waiting beats working beats idle),
and the freshest matching line wins across lines. A `limited` list holds
quota/rate-limit language so a quota wall never reads as a crash. An
`idle` list holds done language (completion banners, all-tasks verdicts)
so a finished agent reads `idle` instead of badging as `waiting` — and
only `waiting` means "blocked on you". `idle_fallback` holds done
*chrome* (input boxes, idle footers): it reads as `idle` only when
nothing else matched — need anywhere in the tail beats it, banners
summarize better, and live activity below it wins (stale activity above
it loses, since scrolled-past output is history). A transition into
`idle` from a live state fires one desktop "done" note (same toggle as
the needs-you notes).
`waiting_unless` vetoes a waiting verdict when any of its patterns match
the tail (an input prompt under a live task is an input box, not a plea),
falling back to the freshest working line or no verdict. Without
a command match, patterns still yield a
state, but the agent stays unnamed unless the screen mentions it: a bare
approve prompt plus a live agent process reads as that agent waiting,
while the same prompt with no process behind it claims nobody. A manifest
with a broken regex is skipped with a warning instead of killing the
daemon.

When the detector misses an agent entirely (unknown binary, odd
wrapping), pin it by hand instead of writing a manifest:

```sh
terhub register %4 --agent myagent [--pid 1234] [--state working]
terhub unregister %4
```

A pin is a `man`-sourced record that survives polls until you drop it
(never lapses, never overwritten by detection — but a real hook event
for the pane takes over). `--pid` lets the poll track CPU under that
process for stuck detection, and the pin drops itself when the process
exits. Handles work: `register @old-pin …` re-pins under a new name.

## Hooks: telling terhub what agents do

Hooks are fire-and-forget, made for agent lifecycle scripts:

```sh
terhub hook --agent claude --pane %1 --state waiting --summary "Approve plan?"
terhub hook --agent pi --pane %2 --state working --prompt "fix the parser"
```

- `--state` accepts `working|waiting|error|idle|stuck|limited|unknown`
  plus aliases: `work/busy/running/start`, `wait/input/attention/blocked/
  needs-input/stop`, `failed/fail`, `done/complete/completed`, `hung`,
  `limit/quota/capped`.
  `start`/`stop` match the lifecycle states that spawning harnesses emit.
- `--pane` is the pane id (`%N`); inside tmux, `$TMUX_PANE` has it.
- `--summary` is free text shown in peek/status and matchable by rules.
- `--prompt` records what the user asked: it lands in the prompt ledger
  (`recap` reads it back) and on the record peek shows. `--prompt-stdin`
  reads it from stdin (a JSON `{"prompt": …}` wins, else the first raw
  line) — both stdin flags can be combined; the pipe is read once.
- `--model` records the model id (`opus-4.5`) and `--ctx` the
  context-window fullness (0–100): both ride the record (polls carry
  them, events that don't know keep the old cut) and show in the TUI
  preview title strip next to agent/state, e.g.
  `%1 · wez:dev · pi working 3m · opus-4.5 · ctx 72%`. Wrappers that
  know their model report it; the stock claude/pi hooks don't set these.
- Unknown states are rejected; if the daemon is down the hook warns on
  stderr but still exits 0 — fire-and-forget, so a hook can never break
  the agent lifecycle script that called it.
- Until the first hook lands, the TUI title shows `no hooks yet`:
  hooks are the only true-state source, everything else is proc/screen
  guessing.

State changes from hooks are logged to the ledger and fire desktop
notifications (unless disabled). Prompts log as their own `prompt`
ledger entries even when the state didn't move — asking again is itself
the event.

`terhub hooks install [claude|pi|all]` wires agents up for you: claude
gets `SessionStart`/`UserPromptSubmit`/`Notification`/`Stop` entries
merged into `~/.claude/settings.json` (idempotent, user hooks kept,
first write backed up to `settings.json.terhub-bak`; re-run install to
gain newly added events), pi gets a lifecycle reporter at
`~/.pi/agent/extensions/terhub.ts` (a stale ours-file is rewritten in
place so hook upgrades land; foreign files are backed up, never
clobbered). The prompt hooks are `UserPromptSubmit` (claude) and
`before_agent_start` (pi, carrying the raw prompt text). `hooks status`
shows what is wired, `hooks uninstall` removes only terhub's entries.
Restart running agents so they pick the hooks up. `--summary-stdin`
reads a hook's summary from stdin (a JSON `{"message": …}` wins, else
the first raw line) for shims whose platform delivers context on stdin.

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
  scripts and status bars. Hooked agents also show their last prompt and
  how long ago you sent it, plus which pane was prompted most recently.
- **`status-line`** prints one line for `status-right`, e.g.
  `set -g status-right '#(terhub status-line)'`. Always exits 0 (prints
  `◎ --` when the daemon is down).

## Tray icon (KDE/Plasma)

```sh
terhub tray
```

Runs a StatusNotifierItem in the system tray until killed (ctrl-c
removes the icon). The icon switches to its attention variant when
something needs you, and the hover tooltip carries the counts:

```
terhub — 8 need you · 2 idle
18 tracked · 8 waiting on you · 2 idle
click to open the hub
```

KDE's tray cannot draw a numeric badge — the StatusNotifierItem spec
has no field for one — so the numbers live in the tooltip. Clicking
opens the hub in a terminal: `konsole`, `foot`, `alacritty`, `kitty`,
or `xterm`, first one found. Override with `TERMHUB_TERMINAL="… -e"`.

Icons are written to `~/.local/share/icons/hicolor/scalable/apps/` on
first run, and the shell icon cache is refreshed when the tooling
exists. A down daemon reads as `terhub — daemon down`, never as a
misleading zero. It is not started automatically with the daemon — run
it yourself (or from a systemd user unit) if you want it always there.

## History: stats, report, activity, timeline

```sh
terhub stats [--since today] [--group-by day|agent|session]
terhub report [--since today] [--json|--markdown]
terhub activity [--since 24h] [--kind agent|session|rule|human|prompt|all] [--json]
terhub recap [--since today] [--pane %N] [--limit 10]
terhub timeline [--since today] [--session S] [--agent A]
  [--sort time|waiting] [--view time|heatmap]
```

`--since` accepts `today|yesterday|week|month|all`, durations like `24h` /
`7d`, or `YYYY-MM-DD` (impossible dates such as 02-31 are rejected, not
normalized). Day boundaries are UTC. `timeline` opens at the newest entries;
`q`/`Esc` quits, arrows/`PgUp`/`PgDn` scroll. `--sort waiting` leads with
the attention states; `--view heatmap` renders a day × hour activity grid
instead of the entry log. `report` adds a foreground section (time your
focus spent per pane, from the `human` ledger) to the three breakdowns;
`--json` and `--markdown` export the same data for scripts and pasting.

`recap` reads back what you asked: recent prompts newest-first with ages,
optionally scoped to one pane — the answer to "what did I tell this agent?".

Entry kinds: `agent` (state transitions), `session` (open/close), `rule`
(automation firings), `human` (your focus moving between panes), `prompt`
(user prompts via hooks). Every
transition is logged (working and idle included), so WORK/IDLE columns
reflect real time — but only from this version on; older ledger ranges
logged attention states only, and show 0 there.

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
`POST /api/interrupt {pane}`. Status answers 503 when the daemon is down;
stats/timeline serve ledger history (frozen while the daemon is down) and
stay 200.

## Fleet (multiple hosts)

Run terhub + daemon on each host, register them locally:

```sh
terhub host add <name> <ssh-target> [--mode observe|control] [--label "Dev Box"]
terhub host list|doctor <name>|remove <name>|label <name> "<label>"
terhub fleet status [--json]
terhub fleet send <host> <pane> <text>
terhub fleet attach <host> [session]
```

Host labels are display names: rows show `label ▸ session`, the hosts
popup and `host list` show the label, but routing ids stay `name:%N`
so labels are free-form and renamable (`host label <name> ""` clears
back to the name).

`fleet status` queries all hosts concurrently over `ssh` and reports each
host's agents (or the ssh error) in config order. `fleet send` only works
on `control`-mode hosts; `observe`-mode hosts refuse. `host doctor` checks
the remote for ssh reachability, `terhub` on PATH, tmux, and a running
daemon. Hosts live in `~/.config/terhub/hosts.toml` (names can't contain
`:` — remote rows are `host:%N`).

In the TUI, `m` opens the fleet hosts popup: `a` adds
(`name target [observe|control]`), `x` removes, `r` fetches every host's
agents into the main table as `host:%N` rows (under `All` only; fetch is
manual — ssh has no place in the 3s tick), `Enter` jumps to the host's
neediest cached agent. Remote rows are read-only in the table: preview
works (captured over ssh) and `Enter`/click jumps out via `ssh -t`
straight into the remote session (the TUI exits — ssh owns the terminal),
but kill/restart/pause/mark/rename and custom commands refuse rather than
act on the wrong machine. `attend` considers cached remotes too.
`fleet attach` is the same jump for scripts (no session = most-recent).

## Native terminal tabs

tmux only sees its own panes; the terminals hosting them have their own
tabs. terhub reads Konsole (over `qdbus`), kitty (`kitty @`, needs
`allow_remote_control` plus `TERHUB_KITTY_TO` pointing at the socket when
run from outside kitty — without either the probe is skipped outright,
since `kitty @` would otherwise wait 10s on the terminal), and WezTerm
(`wezterm cli`, never auto-starting a server) as first-class rows. Every
backend call carries a 5s timeout, so a wedged helper degrades to "no
tabs", never a frozen TUI:

```sh
terhub native list [--json]
terhub native spawn konsole|kitty|wezterm
terhub capture konsole:<kpid>:<sess> [--lines N]
terhub focus kitty:3
terhub send wezterm:7 "ls"   # --no-submit skips Enter
```

In the TUI, native tabs are fetched at startup and on `F5` (the 3s tick
leaves them alone — tab enumeration is subprocess-heavy) and show under
`All` only, with the backend as the session (`konsole:1535082:6`). Jump
switches the terminal's tab and stays open; on Konsole the jump also
raises the window through a one-shot KWin script (tab-select alone is
invisible while Konsole sits behind other windows) — if the raise
fails the error says the tab was still switched. Preview, pause, and
custom commands work; kill/restart/mark/rename refuse (Konsole exposes
no tab-close over DBus). Agent badges come from the process tree plus one
screen classification at fetch time — shells in those tabs have no hooks,
so states are screen-truth, frozen until the next fetch. `Idle` tracks
visible screen change across fetches (same denoise as the daemon, so
spinners don't restart it); first sighting starts fresh. Konsole capture
is the visible screen (its scrollback offsets are undocumented);
kitty/WezTerm capture honors `--lines`. Fleet host names can't collide
with backend names (`konsole`, `kitty`, `wezterm` are reserved).

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
| `init [--dry-run]` | One-shot setup: config + hooks + daemon |
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
| `send <pane> --template <name>` | Type a `[[template]]` snippet instead |
| Pane args | `%N`, native ids, `host:%N`, and `@handle` all work |
| `status [--json]` | Agent states from the daemon |
| `hook --agent A --pane P --state S [--summary T] [--prompt P]` | Push a hook event |
| `recap [--since S] [--pane P] [--limit N]` | Recent prompts, newest first |
| `hooks install\|status\|uninstall [agent]` | Wire agent lifecycle hooks |
| `daemon start\|stop\|status` | Manage `terhubd` |
| `attend [--cycle] [--list]` | Jump to longest-waiting agent |
| `peek [--plain]` | This window's panes + states |
| `status-line [--pane P]` | One line for tmux status-right |
| `tray` | KDE/Plasma tray icon with live counts |
| `stats [--since S] [--group-by G]` | Time-in-state stats |
| `report [--since S] [--json\|--markdown]` | All breakdowns + foreground |
| `activity [--since S] [--kind K] [--json]` | Raw ledger entries |
| `timeline [--since S] [--session S] [--agent A] [--sort S] [--view V]` | Timeline log or heatmap |
| `automation list\|test <name>` | Rules: validate / dry-run |
| `mcp` | MCP stdio server |
| `serve --port P` | Loopback web dashboard |
| `host add\|list\|doctor\|remove` | Fleet host registry |
| `fleet status [--json]` | Agents across hosts |
| `fleet send <host> <pane> <text>` | Drive a control-mode host |
| `fleet attach <host> [session]` | SSH into a host's tmux |
| `native list [--json]` | Tabs across Konsole/kitty/WezTerm |
| `native spawn <backend>` | Open a fresh native tab |
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

