# terhub guide

terhub is a terminal command center: one fullscreen TUI (plus a plain CLI)
that gathers every session, pane, and agent from every place you work
(local tmux, local zellij, local GNU screen, SSH hosts, agent processes)
into one screen, names which ones need you, and lets you jump to them,
mark them, and drive them — from scripts, rules, MCP clients, a loopback
web dashboard, or other hosts over SSH. Full Rust, mouse-friendly,
non-vim, and every key rebindable.

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
- [GNU screen baseline](#gnu-screen-baseline)
- [Command reference](#command-reference)
- [Files and locations](#files-and-locations)
- [Troubleshooting](#troubleshooting)

## Install

Requires tmux 3.x on PATH and Rust 1.89+.

```sh
cargo install --path .
terhub init       # config + agent hooks + daemon, one shot
terhub            # opens the TUI
```

For day-to-day development, prefer `make build` (or just `make`): it
runs `cargo build` and then `cargo install --path . --offline --locked
--force` so the binary on `$PATH` (`~/.cargo/bin/terhub`) stays in
sync with the working tree. A stale `~/.cargo/bin/terhub` against a
fresh `target/debug/terhub` is the most common "is it broken or is
the binary old?" confusion; `make build` removes that footgun. Other
targets: `make test`, `make clippy`, `make install` (install only),
`make build-release` (release build + install), `make check-lock`
(lockfile/registry sanity), `make check-install` (proves the installed
binary is fresh — also a CI gate), `make clean`.

`terhub` with no arguments opens the TUI; every subcommand below also works
from scripts. First run creates `~/.config/terhub/config.toml` with defaults.
`init --dry-run` prints the three steps without running them.

## Concepts

- **Session / window / pane** are tmux's own. terhub never wraps or renames
  them; pane ids look like `%69`.
- **Agent** is anything running in a pane that behaves like a coding agent
  (claude, codex, gemini, pi, …). terhub tracks each agent pane in one of
  nine states: `working`, `waiting` (needs you), `error` (needs you),
  `idle`, `done` (finished — a completion banner was seen; never needs
  you), `stuck` (needs you: no CPU for 10+ minutes), `limited` (needs
  you: quota/rate wall, paused until reset — never a crash), `paused`
  (you stopped it — never needs you), `unknown`.
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
  screen guess), age, last-turn duration, activity, summary. When
  `[ui] activity_sparkline = true`, an extra `Activity` column holds a
  six-cell output sparkline (rightmost cell = the newest tick, taller
  = more bytes printed), sampled every few seconds and kept in memory
  only. The summary shows the prompt or
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
  tree. A frozen pane reads as `paused`: it is dim, it never counts as
  "needs you", and it is never reported as stuck (a stopped process burns
  no CPU, which is exactly what the stuck smell looks like — see
  [Pausing a pane](#pausing-a-pane)). `Space` toggles a `+`
  mark on the row and steps down (file-manager style), `u` clears all
  marks; with marks set, `x` kills the whole marked set under one
  confirmation and `a` attends the longest-waiting marked pane. Marks
  survive sort/filter and show as a count in the title bar. Any pane
  you reach with `Enter` or `a` also records the visit: the row wears a
  quiet `✓` until its state or summary changes — so a sweep of the
  table shows what you have not looked at yet. A pane that still needs
  you keeps its `!` (the tray still counts it), and `+`/`*` outrank the
  check. Long lists scroll;
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
- **Filter (temporary search) vs standing filter.** Two different things
  with two different labels. *Search*: `f` opens the prompt, but you
  rarely need it — just start typing and any unbound printable key
  opens the prompt with that char already entered (`/` always forces
  it, e.g. for queries starting with a bound key). It narrows rows by
  space-separated tokens, OR-combined, each a case-insensitive
  substring across every text column (session, window,
  command, dir, agent, state, summary) — `vim npm` shows rows
  mentioning either. Prefix a token with `!` to exclude it instead
  (`vim !zsh` hides every vim row mentioning zsh). Typing filters live,
  `Enter` keeps the query **for this session only — it is never saved**,
  `Esc` clears it.
  *One exception*: a query made of nothing but `!` terms (`!zsh !micro`)
  is not a search — it says "hide these", and that is a preference, not
  a question. Such a query is **merged into the standing filter and
  saved to the config**, so it survives the next launch; the status
  line says `exclusions saved (remembered): zsh micro`. A query with
  any positive term (`vim !zsh`) stays a session-only search.
  *Filter chip*: while any filter is active, the table header carries a
  `◼` block plus a label naming every filter that is hiding rows —
  `◼ filter: zsh · search: vim` — so a filter is never silent. It rides
  the title line when there is room and drops to a header line of its
  own above the table on narrow terminals, rather than being clipped.
  *Standing filter*: the always-on **exclusion** from `[ui] filter`, shown
  as `filter:` in the title. Whatever you type in its editor is saved
  as an exclusion (`zsh` becomes `!zsh`) — the standing filter only
  ever hides, so typing it can never surprise you by selecting. The
  editor opens with the `!` already in place and previews the
  exclusion live under an `Exclude filter / Hide:` prompt, distinct
  from the search's `Filter / Text:` prompt. Edit it in settings (`s` → Filters → standing filter,
  `Enter` saves to the config file, `Esc` cancels). `Esc` in the table clears the search, never the
  standing filter. It composes with the session filter. Bind
  `filter_clear` (e.g. `ctrl+backspace`) for a hard wipe of the search
  query inside the prompt — `Esc` alone always restores, never clears.
  `h` hides unclaimed panes quiet over 10 minutes (one key, reversible,
  hidden count in the title) — the fast way to drop idle shells from a
  dashboard view.
- **Named (saved) filters.** `ctrl+f` cycles the saved filters in
  `[ui] saved_filters`, wrapping past the last one back to the first;
  the active name shows as `filter:` in the chip (a hand-typed query
  keeps showing `search:`). Manage them in settings (`s` → Filters):
  `Enter` on a row applies it, `n` names the current search, `r`
  renames, `d` deletes, and every change is written to the config.
  A saved filter composes with the standing exclusion, and typing an
  edited query drops the name until you type the saved query exactly
  again. The naming prompt is one line, like every other prompt.
  Prompts eat all clicks; the settings page rows and its sidebar are clickable.
- **Refresh.** `F5` re-reads tmux and re-fetches native terminal tabs
  (Konsole/kitty/WezTerm show as `konsole:…` rows under `All` only; see
  [Native terminal tabs](#native-terminal-tabs)).
- **Bulk prompt.** `b` sends one message to every marked pane — or, with
  no marks, to every agent waiting on you. `Tab` cycles `[[template]]`
  snippets into the text; `Enter` sends with submit (these are prompts,
  not drafts). Remotes and your own pane are never targets; failures are
  counted in the footer.
- **Styles.** `t` cycles the twenty built-in themes and `v` flips table
  density (comfortable shows the summary column, compact drops it and
  tightens columns) — both session-only try-outs. `s` opens the full
  settings page: a sidebar (Theme, Density, Toggles, Filters, Tray
  icons) with the section's rows beside it. `↑`/`↓` move (across
  sections), `←`/`→` jump between sections, `Enter` applies, `Esc`
  closes; clicking a sidebar entry jumps straight there. Theme and
  density apply live; the toggles and the standing filter save to the
  config file. Next to the Theme and Density lists, a preview box
  renders sample rows in the hovered style — browse themes and
  densities with live feedback before applying, even on an empty
  setup. **Toggles** also include *blur columns* (record/share mode):
  fills the Session, Window, Cmd, Dir, Agent, Handle, and Summary
  cells — plus the session-bar tab names — with a solid `█` of the same
  width, so a recording or shared shot shows which agents need you and
  how many are idle without revealing the repos, commands, or prompts
  being worked on. State, source, counts, and the marks stay readable.
  No keybind by design: it is obvious when the toggle is on, and it
  would be easy to forget a privacy-off switch is armed.
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
  Unclaimed panes fall back to tmux's window-activity age. Every row
  shows that age, `working` rows included: it is what separates
  "streaming" from "frozen on the same frame". A `working` row is never
  *tinted*, though — a long quiet turn (a model thinking, a tool call
  that prints nothing) is not an idle one, so only a genuinely `idle`
  row tints; its turn age also lives in the `Turn` column, and a hung
  agent shows `stuck` in the State column. Agent cells share one color
  per agent name (see `[style]`), and `Idle` tints when a row is `idle`
  but quiet past `idle_flag_secs` (default 6h, `0` disables), without
  relabeling the state.

Everything except arrows/mouse/Tab/Esc/Alt+digits is rebindable (see
[Configuration](#configuration)); the `?` help always shows your current
bindings (press `?` again for the legend: marks, `Src`, `For`/`Turn`/`Idle`).
Footer confirmations fade after a few seconds; the title names a dead
daemon instead of silently emptying the agent columns — and that same
refresh starts the daemon again (see [The daemon and agent
awareness](#the-daemon-and-agent-awareness)).

## Configuration

`~/.config/terhub/config.toml` (see `terhub config --show`, `--path`,
`--init`):

```toml
[ui]
theme = "dark"       # dark | light | dracula | nord | gruvbox | solarized | tokyonight | catppuccin | onedark | monokai
density = "comfortable"  # comfortable | compact
highlight_attached = true
mru_tabs = false     # order session tabs by recent activity
blur_cols = false    # record/share mode: fill Session/Window/Cmd/Dir/Agent/Handle/Summary with █
                     # so a recording or shared screenshot hides what you're working on
                     # while state, src, counts, and the marks stay readable (s → Toggles)

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
# Cycle the named filters in [ui] saved_filters (wraps).
saved_filter = "ctrl+f"
# Optional hard-clear for the filter query. Unbound by default; bind
# e.g. filter_clear = "ctrl+backspace" to wipe the query mid-prompt.
# Esc in the filter prompt always restores the pre-edit query.
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
quiet_idle_secs = 900 # frozen screen → idle (0 disables)
waiting_idle_secs = 1800 # untouched prompt → idle (0 disables)
done_idle_secs = 300 # stale completion → idle (0 disables)

[style]
color_agents = true  # hash-color Agent cells so one agent reads as a group
agent_palette = []   # style specs for the hash; empty = theme default
idle_warn = true     # tint the Idle cell of a long-idle row
idle_warn_style = "bold yellow"
idle_flag_secs = 21600  # tint Idle when idle + quiet past this (0 disables)

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

Key names: single characters, `enter`, `esc`, `tab`, `f5`, …, and
modifier prefixes `ctrl+`, `alt+`, `shift+`, `super+` (also spelled
`control`/`option`/`meta`/`cmd`) — e.g. `ctrl+s`, `alt+f5`,
`ctrl+shift+enter`. Modifier-qualified bindings fire only on that exact
combo and never shadow the bare key; unmodified presses of a bare
binding keep their old behaviour. Unknown keys fall back to defaults. Notifications gate the desktop popups on agent state
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
`already running` and exits 0).

Nothing supervises the daemon, and it can still die: a terminal's SIGHUP,
or the OOM killer picking the process that used to sit under a heavy
parent. A dead daemon used to stay dead — every TUI then showed `daemon
down` with the agent columns empty for the rest of the session, until
someone thought to run `terhub daemon start` by hand. The TUI now
self-heals: the first refresh that cannot reach the daemon starts it
(own process group, stderr to `terhubd.log`), rate-limited to one attempt
per 2s so an unbootable daemon is retried instead of fork-storming. The
title keeps saying `daemon down` until the next good snapshot, so the
error is never hidden — it is just repaired too.

### Client and daemon must be the same build

`terhub daemon status` prints both identities side by side:

```sh
$ terhub daemon status
running (/run/user/1000/terhub/terhubd.sock)
daemon build 0.3.0+1196a1687bfda34a (proto 1)
client build 0.3.0+1196a1687bfda34a (proto 1)
```

The *build* is a fingerprint of the sources the binary was built from
(the same one in every binary of the package), and *proto* is the wire
protocol version. The two must agree: a client that cannot decode what
the daemon says would otherwise look exactly like a daemon that is not
running, and the TUI would show `daemon down` with empty columns while a
healthy daemon kept serving. It does not:

- The client sends its protocol version; the daemon answers with its own.
  A mismatch is reported by name — `daemon build incompatible: client
  proto 1 / daemon proto 2 — run `make build``.
- A client from *before* the protocol was versioned keeps working: the
  daemon recognizes the old request shape and answers it in a shape that
  client can read (states it does not know are down-projected to the
  nearest state it does).
- A state this build has never heard of decodes as `unknown` and the
  pane stays listed, instead of failing the whole snapshot.
- The TUI distinguishes `daemon down` (no daemon — start one) from
  `daemon incompatible` (a daemon answered, this client cannot read it
  — rebuild). It never re-forks the daemon for the second case: the
  daemon is already running, and re-forking it only fills the log with
  "another terhub daemon already holds the lock".

So a mismatch is fixed by one command: `make build` (build + install,
client and daemon together), then `terhub daemon stop && terhub daemon
start` so the running daemon is the one you just built.

Per pane it merges three signals:

1. **Hook events** (`terhub hook …`, see below) — authoritative while fresh.
   A hook record with no new event for `stale_secs` (default 30 minutes)
   lapses back to automatic classification, keeping its clock if the state
   didn't change (no renotify for a pane that sat still) but dropping its
   stale summary. A hook is authoritative for the agent it names, not for
   the pane forever: if the process tree identifies a different live agent,
   the pane has been reused (exit one agent, start another in it) and the
   live agent replaces the ended agent's hook immediately.
2. **Process tree** — the pane's descendants are matched against the agent
   catalog (claude, codex, gemini, pi, muse, aider, crush, opencode, …),
   seeing
   through wrappers like shells and `systemd-inhibit`. Aliased frontends
   resolve to their engine (`agy` → gemini, `claude-code` → claude).
   Session harnesses
   (muxa, tmux) are transparent: detection looks *through* them for the
   real agent. This decides
   *who* the agent is; a live agent with an unreadable screen reads as
   `working`. Its subtree CPU is tracked: `working` with no CPU for
   `stuck_secs` (default 10 minutes) becomes `stuck` (needs you), clearing
   the moment it computes again (`stuck_secs = 0` disables stuck
   detection). A live agent whose **screen has not changed** for
   `quiet_idle_secs` (default 15 minutes) becomes `idle` — a finished turn
   prints whatever it prints, and most print nothing that matches an idle
   pattern, so without this they read `working` forever (their REPL keeps
   burning CPU, so stuck detection never trips either). The screen moving
   again flips them straight back to `working`; `0` disables it. Stuck is
   still CPU-based, so it outranks the quiet flip and an `idle` row can
   still escalate to `stuck`. A `waiting` row works the same way but on a
   longer clock: `waiting_idle_secs` (default 30 minutes). Agents whose
   CLI ends every turn with a bare `>` (gemini) classify as `waiting`
   forever once the work is done, so without this they sit in "need you"
   long after you walked away. The longer threshold is deliberate —
   stepping away for a few minutes must not silence a real question —
   and the summary is kept, since that is what you came back to read.
   A `done` row decays the same way, and faster: `done_idle_secs` (default
   5 minutes). `done` is the transient FYI for a finished turn — it floats
   above the idle mass when the table is sorted by state, so a completion
   you missed still surfaces. It is not a resting state. The verdict comes
   from a banner that stays in the scrollback, so without the decay the
   classifier re-matches it on every poll and the row reads `done` forever:
   it never counts towards the `N idle` summary, and to anyone scanning
   the column it looks like an agent mid-turn. The shorter threshold is
   deliberate — a completion banner is positive evidence that the turn
   ended, where a frozen screen is a guess that has to wait out a false
   positive — and the summary is kept, so the completion itself is not
   lost; only the label decays.
   Each `0` disables its flip. Fresh hooks are never second-guessed this
   way.
3. **Screen patterns** — built-in manifests for claude/codex/gemini/muse
   match the pane's command and recent screen text. The freshest matching
   line decides, so current output overrides stale scrollback; `waiting`/
   `error` on screen win over the process state. Harness dashboards (muxa,
   terhub's own TUI) skip screen classification — they name agents
   without running them. When no manifest matches the command the
   classifier still tries every manifest over the screen (an agent
   without its own manifest still gets a verdict), naming the agent only
   when the screen mentions it. `pi` has its own manifest, claiming only
   the attention states: its bottom chrome (git line, the ↑/↓/cost stats
   line, the Chrome Bridge line) renders identically while the model
   streams and after the transcript stops, so `working` and `idle` stay
   with the process tree and the quiet clock instead of screen text.
   A record with no supporting evidence for 30
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
done = ["task complete", "all done"]
idle_fallback = ["enter send"]
waiting_unless = ["esc to interrupt"]
busy = ["background task running"]
phase = ["working on it"]
```

Patterns are case-insensitive regexes; within one line the most specific
signal wins (limited beats error beats waiting beats working beats done),
and the freshest matching line wins across lines. A `limited` list holds
quota/rate-limit language so a quota wall never reads as a crash. A
`done` list holds completion language (completion banners, all-tasks
verdicts) so a finished agent reads `done` instead of badging as
`waiting` — and only `waiting` means "blocked on you". (The legacy
`idle` list is still accepted as an alias for `done` when no `done`
list is present; new manifests should use `done`.) `idle_fallback` holds done
*chrome* (input boxes, idle footers): it reads as `idle` only when
nothing else matched — need anywhere in the tail beats it, banners
summarize better, and live activity below it wins (stale activity above
it loses, since scrolled-past output is history). A transition into
`done` from a live state fires one desktop "done" note (same toggle as
the needs-you notes).
`waiting_unless` vetoes a waiting verdict when any of its patterns match
the tail (an input prompt under a live task is an input box, not a plea),
falling back to the freshest working line or no verdict.
`busy` and `phase` cover the other half of the problem: work that is
still in flight after the agent's own turn ended. A `busy` pattern (a
running goal, a live background task) is working evidence *and* a veto —
it outranks the quiet verdicts that describe a turn which ended, so a
pane whose todo list reads `4/4 completed` while its goal is still
verifying does not read as `done`. A `phase` pattern is the same, plus
it supplies the Summary text: a busy pane has no verdict to show, and
the phase (`◎ Goal · Verifying the result · 54min59s active`) is the one
line that says what the agent is up to. Without a command match, patterns still yield a
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

### Pausing a pane

`z` in the TUI freezes the pane's **agent** with `SIGSTOP` (and `z` again
thaws it). A frozen pane is **paused**, and terhub remembers that it is
the one that paused it:

- The row reads `paused` and renders dim — never red, never a `!` badge,
  never counted in "needs you". You froze it on purpose; nobody needs to
  be told about a hang that does not exist.
- It is **not** reported as `stuck`. A stopped process burns no CPU,
  which is byte-for-byte the same signal the stuck detector reads as
  "hung" — so without the pause registry, terhub would escalate an
  alarm about its own `z` press.
- The freeze targets the **agent's own process group**, not the pane
  root's. That matters under tmux: a pane root sits in its own process
  group and an interactive shell gives its foreground job another one,
  so signalling the pane root never reaches the agent at all — and
  tmux resumes a pane whose root stopped, so the freeze would not even
  hold. The pane's shell stays alive and responsive while the agent is
  frozen; a pane with no detected agent falls back to freezing its
  root.
- The mark is read from the process state (`/proc/<pid>/stat`) on every
  poll, never carried, so it can never go stale: thaw the pane and it
  is working again on the next tick, kill it and the mark goes with it.
  The same is true for a pane stopped by anything else — `kill -STOP`
  from a shell, a `Ctrl-Z`, a suspend, an editor — not just by terhub's
  `z`. A paused agent is the one thing terhub *never* escalates about.
- Thawing is clean: the freeze's silence is not inherited, so a resumed
  agent is not reported as `stuck` on the way back. The still-clock
  measured the pause, not the agent's work.
- The pause is persisted in the agent registry, so it survives a daemon
  restart: the pane does not come back as `stuck` the moment terhubd
  bounces.
- Paused time counts as **idle** in `terhub stats` and `report` — it is
  neither work nor an error, and the report schema stays the same.

The pause clock is the moment the pane was first seen frozen, and it
keeps running while the pane stays frozen ("paused for 2h"), so the
state column tells you how long you left it hanging.

## Hooks: telling terhub what agents do

Hooks are fire-and-forget, made for agent lifecycle scripts:

```sh
terhub hook --agent claude --pane %1 --state waiting --summary "Approve plan?"
terhub hook --agent pi --pane %2 --state working --prompt "fix the parser"
```

- `--state` accepts `working|waiting|error|idle|stuck|limited|paused|unknown`
  plus aliases: `work/busy/running/start`, `wait/input/attention/blocked/
  needs-input/stop`, `failed/fail`, `done/complete/completed`, `hung`,
  `limit/quota/capped`, `paused/stopped/frozen/suspended`.
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
merged into `~/.claude/settings.json` (idempotent across all matchers,
user hooks kept, first write backed up to `settings.json.terhub-bak`;
re-run install to gain newly added events), pi gets a lifecycle
reporter at `~/.pi/agent/extensions/terhub.ts` (a stale ours-file is
rewritten in place so hook upgrades land; a foreign file at that path
is moved aside to `terhub.ts.terhub-bak` so we never overwrite it).
The prompt hooks are `UserPromptSubmit` (claude) and
`before_agent_start` (pi, carrying the raw prompt text). pi also reports
`waiting` while it blocks on the human: question tools (`question`,
`questionnaire`, which carry the question text as the summary) and any
other blocking UI prompt (permission confirms, extension dialogs), then
`working` again when the answer lands — otherwise a pane parked at a
question menu keeps the `working` verdict its last `turn_start` reported,
since `agent_end` never fires while the agent is blocked. `hooks status`
shows what is wired (a stale ours-file reads as `stale`; reinstall to pick
up newer events), `hooks uninstall` removes only terhub's entries.
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
removes the icon). Six icon variants — one mark, three color
treatments, two live overlays:

| Icon | Meaning |
|---|---|
| `terhub` | daemon up, no idle, no hub open, nothing needs you |
| `terhub-idle` | long-running — agents finished, daemon up, no hub |
| `terhub-attention` | something needs you (Waiting/Error/Limited/Stuck > 0) |
| `terhub-disconnected` | daemon unreachable — gray with strikethrough |
| `terhub-live` | a hub is already open, agents busy |
| `terhub-live-idle` | a hub is already open and every agent has finished |

The eye learns three signals: red ⇒ needs you, teal ⇒ agents done,
gray + strike ⇒ daemon down. A green corner dot layered on a calm or
idle base says "a hub is already running — clicking focuses it".

**Themed.** The mark is the same shape in every theme, but the colors
pull from the active `theme` config (dracula's cyan + magenta,
gruvbox's amber + orange, nord's frost + purple, etc.). The alarm
badge uses the theme's existing accent so a "needs you" icon in nord
is rust-red, in gruvbox it's bright red, in catppuccin it's pink — the
icon is always part of the panel, never bolted on.

The hover tooltip carries the counts:

```
terhub — 8 need you · 2 idle
18 tracked · 8 waiting on you · 2 idle
click to open the hub
```

**Clicking** never opens a second hub. If one is already running the
icon focuses it — `tmux switch-client` on its pane when it runs in
tmux, otherwise the terminal window owning its tty (via `xdotool`).
With no hub open, it starts one: `konsole`, `foot`, `alacritty`,
`kitty`, or `xterm`, first one found. Override with
`TERMHUB_TERMINAL="… -e"`.

A running TUI advertises itself in
`$XDG_RUNTIME_DIR/terhub/hub-session.json` and clears it on exit; a
file left behind by a killed hub is ignored (its pid is checked) and
removed on sight.

KDE's tray cannot draw a numeric badge — the StatusNotifierItem spec
has no field for one — so the numbers live in the tooltip. A down
daemon reads as `terhub — daemon down`, never as a misleading zero. It
is not started automatically with the daemon — run it yourself (or
from a systemd user unit) if you want it always there.

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

### A torn ledger

```sh
terhub history status     # size, line count, quarantined lines
terhub history repair     # move the torn lines into an archive
```

A crash can cut a ledger line in half. The prune keeps such a line as a
`__corrupt__` entry — the data is never dropped — but a hot ledger full
of them is a ledger nobody can read, and their count hides new damage in
the noise of old damage. `terhub history repair` moves them out: the hot
ledger parses cleanly again, and the torn lines are preserved beside it
in `activity.jsonl.quarantine.jsonl`, with the raw head, the parse
reason, and the timestamp the line carried. Nothing is reconstructed — a
line that was cut in half cannot honestly be completed, and inventing the
missing half would fabricate history.

`terhub history status` always reports the count, so "0" is something
you can check after a repair.

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
text = "continue"          # send-keys payload (submit = false types without Enter)
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
- `terhub_history` — the activity ledger: state transitions, prompts, session
  events and rule fires.
- `terhub_capture` — recent screen content of a pane.
- `terhub_send` — type text into a pane (submits with Enter by default).
- `terhub_wait` — block until a pane (or any agent) reaches a state; times
  out with an error instead of hanging forever.
- `terhub_attend` — the longest-waiting agent needing attention.

`terhub_status` and `terhub_history` answer with JSON, so a caller never has
to parse prose. `terhub_status` returns `{"count": N, "agents": [...]}` where
each agent carries `pane`, `agent`, `state`, `state_since`, `waiting_secs`,
`session`, `window`, `handle`, `model`, `ctx_pct`, `last_turn_secs`,
`summary`, `source` and `needs_attention`. It filters on `state`, `session`,
`pane`, `agent` and `needs_attention`, caps the list with `limit`, and
`{"format": "text"}` brings back the old one-line-per-agent form.

`terhub_attend` answers in the same shape — `{"attending": {...} | null,
"agents": N}` — so one reader parses both. `{"format": "text"}` gives the old
one-liner.

`terhub_history` returns `{"count": N, "since": <ts>, "entries": [...]}`.`since` takes the same words as the CLI (`today`, `yesterday`, `week`,
`month`, `all`, `24h`, `7d`, `YYYY-MM-DD`; default `today`), and `kind`,
`pane` and `session` narrow the set. `{"prompts": true}` returns the recent
prompts instead of the transitions, and `{"stats": true}` adds the
per-agent time-in-state totals.

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
daemon. Hosts live in `~/.config/terhub/fleet.toml` (names can't contain
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

Footer treatment of native probes: a backend that is simply *absent*
(no Konsole on the bus, not inside kitty, no WezTerm server) is an
informational note, not a warning — it never pins the footer and never
uses the alert color. Only real probe failures (missing binary, wedged
CLI, malformed output) surface as a sticky status line, and even those
re-assert only when the text actually changes, so an unchanged error
doesn't repaint the footer every 3s tick.
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

## GNU screen baseline

```sh
terhub screen list|attach <name>
```

Lists and attaches to GNU screen sessions when `screen` is on PATH. Same
listing-baseline scope as zellij: pane observation, agent awareness, and
automation are tmux-backed.

## Backend parity

The same TUI runs against every backend, but only tmux is fully wired
through. The matrix below is the honest state of v0.2.3; anything marked
"–" is on the roadmap and tracked in the project notes, not silently
broken.

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
| `host add\|list\|doctor\|remove\|label <name> [L]` | Fleet host registry (`label` sets/clears the display label) |
| `register` / `unregister` | Pin / drop a manual agent record (lives in the daemon's registry) |
| `fleet status [--json]` | Agents across hosts |
| `fleet send <host> <pane> <text>` | Drive a control-mode host |
| `fleet attach <host> [session]` | SSH into a host's tmux |
| `native list [--json]` | Tabs across Konsole/kitty/WezTerm |
| `native spawn <backend>` | Open a fresh native tab |
| `zellij list\|attach` | Zellij baseline |
| `screen list\|attach` | GNU screen baseline |
| `config --show\|--path\|--init\|--check` | Config file |

## Files and locations

| Path | What |
|---|---|
| `~/.config/terhub/config.toml` | Keys, theme, notifications |
| `~/.config/terhub/automation.toml` | Automation rules |
| `~/.config/terhub/fleet.toml` | Fleet hosts |
| `~/.config/terhub/manifests/*.toml` | Extra agent screen-patterns |
| `~/.local/share/terhub/activity.jsonl` | Activity ledger (90-day prune) |
| `$XDG_RUNTIME_DIR/terhub/` | Socket, pid, registry cache |

`TERHUB_RUNTIME_DIR` / `TERHUB_DATA_DIR` / `XDG_*_HOME` override locations
(empty values are ignored). The registry cache is rebuilt from tmux on every
daemon start; only the ledger is long-term memory.

The TUI remembers its view in `[ui]`: `theme`, `density`, toggles, `filter`
(standing exclusion), `sort` + `sort_desc` (column and direction),
`hide_shells`, and `blur_cols` (record/share mode). Every change writes
back surgically — change the sort with `o` or a header click, hide
shells with `h`, flip blur columns for a recording, quit, relaunch, and
the table looks exactly as you left it.

`TERHUB_TMUX_SOCKET` selects a private tmux socket: a bare name is
`tmux -L <name>` (socket under `$TMUX_TMPDIR`), a value with a `/` is
`tmux -S <path>`. Unset means your live server, exactly as before. Use it
for tests and agent fixtures — `TERHUB_TMUX_SOCKET=terhub-test cargo test`
runs the whole suite on an isolated server (start one if you like; a
missing socket reads as zero sessions). Never `tmux kill-server` the
default one to "isolate" a test: it destroys live sessions.

The config file is safe the same way: a test build resolves the ambient
config to a scratch file under the temp dir, so `cargo test` can never
rewrite your keys, theme or standing filter. A test that wants a
specific config names one — `TERHUB_CONFIG`, or the per-thread override.

## Troubleshooting

- **"daemon not running"**: `terhub daemon start`. Only one daemon per
  runtime dir; a second start says "already listening". A failed start
  prints the tail of `$XDG_RUNTIME_DIR/terhub/terhubd.log` with the reason.
- **"daemon incompatible" in the TUI title, or "daemon build
  incompatible"**: the daemon is running, but the client cannot read
  what it says — two different builds. `terhub daemon status` prints both
  identities; `make build` then `terhub daemon stop && terhub daemon
  start` puts them back in step. A client from before the protocol was
  versioned keeps working: the daemon serves it the old shape.
- **"daemon running but the response could not be read"**: the daemon
  answered, but the answer is not readable by this client. Same fix as
  above, and the message deliberately does *not* say "daemon not
  running": a daemon that answered must never be reported as absent.
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
- **Test killed my tmux session**: an old test setup ran
  `tmux kill-server` with no socket flag. Fixed: the `Tmux` client honours
  `TERHUB_TMUX_SOCKET` (see above), so tests run on a private socket and
  never see the live server.
- **`serve` unreachable**: it only binds loopback by design; open it on the
  same machine (or SSH-forward the port).
- **Times look off by hours**: ledger day boundaries are UTC; local display
  may differ by your timezone offset.

