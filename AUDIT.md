# terhub — audit 2026-10-19

A fresh audit pass over the tree at `main` @ `680acfd` (v0.2.3). Five prior
audit passes are recorded in `.pi-glla/audit-loop/findings.md` (2026-10-08 →
2026-10-17) and fixed ~120 defects. This pass does **not** re-report those;
it covers ground the earlier passes did not: **test hygiene, resource
lifecycle, ledger scale, and the state of the improvement backlog**.

## Findings summary

| # | Severity | Area | One line |
|---|---|---|---|
| A-1 | HIGH | release | `cargo deny` is red — CI and every release are blocked |
| A-2 | HIGH | tests | two suites are not hermetic: one is flaky, both pollute the live ledger |
| A-3 | HIGH | tests | the suite leaks `terhubd` processes that run forever |
| A-4 | HIGH | ledger | every query reads and parses the whole file; no size cap |
| A-9 | HIGH | ledger | `prune()` discards its own write error — retention silently stops |
| A-10 | HIGH | ledger | `prune()` is an unlocked read-modify-write — concurrent appends are lost |
| A-31 | HIGH | tui | Backspace in the new-session prompt panics the TUI (four keystrokes) |
| A-5 | MED | ops | `atomic_write` orphans its temp file; 26 MB already on disk |
| A-6 | MED | web | the dashboard's write endpoints have no auth and no readonly mode |
| A-7 | MED | daemon | the daemon socket has no credential check (the last open ledger box) |
| A-11 | MED | security | the ledger is 0644 while the daemon log is deliberately 0600 |
| A-12 | MED | automation | a failed fire-append silently disables the cap and cooldown |
| A-13 | MED | automation | a ledger read error empties the fire history — guardrails reset |
| A-14 | MED | daemon | the hook client waits 1 s while the server fsyncs inline — events drop |
| A-15 | MED | hooks | the pi reporter's argv breaks on a summary starting with `-` |
| A-16 | MED | hooks | installing rewrites the user's `settings.json` with sorted keys |
| A-17 | MED | hooks | `pi_install` overwrites a foreign file; `pi_uninstall` deletes it |
| A-18 | MED | backends | `screen` is the only backend with no timeout, and it hides failures |
| A-19 | MED | release | the tag workflow skips `cargo deny` and `check-install` |
| A-20 | MED | release | the declared MSRV 1.88 is never built |
| A-8 | LOW | repo | repo and doc hygiene |
| A-21 | LOW | mcp | the state filter's message lists 6 of the 8 real states |
| A-22 | LOW | hooks | the installed hook exits 1 outside tmux |
| A-23 | LOW | web | `/api/capture` echoes the anyhow chain despite `fail()` |
| A-24 | LOW | tray | the tray rescans all of `/proc` every 3 s when no hub is open |
| A-25 | LOW | tray | the tray leaks an unreaped child per click |
| A-26 | LOW | web | the dashboard re-parses the whole ledger twice per 2 s tick |
| A-27 | LOW | tray | the docs describe a badge/strikethrough the renderer never draws |
| A-28 | LOW | fleet | `host doctor` discards the ssh error |
| A-29 | LOW | ops | crash-orphaned temp files in three places, never cleaned |
| A-30 | LOW | web | the timeline API returns oldest-first; the guide says newest-first |
| A-32 | MED | tui | the sparkline's 64 KB tail slice can land mid-character |
| A-33 | MED | tui | the 3 s tick re-captures the preview synchronously (10 s SSH on the UI thread) |
| A-34 | MED | tui | three of four "clear the search" paths leave the filter chip stale |
| A-35 | MED | tui | deleting the last saved filter wedges the Settings popup |
| A-36 | LOW | tui | the mouse-hover highlight is wiped by every 3 s tick |

---

## What was run

| Gate | Result |
|---|---|
| `cargo clippy --offline --all-targets -- -D warnings` | clean |
| `cargo fmt --check` | clean |
| `cargo test --offline` | **1 failure** — `tests/mcp.rs`, flaky (see A-2) |
| `cargo test --offline --no-fail-fast` | 662 passed / 0 failed (the same suite passed on this run) |
| `cargo deny check` | **FAILED** — advisories (see A-1) |
| `cargo publish --dry-run` | not run (needs a clean tree) |

Scale of the tree: 20 source files, 40,431 lines of Rust, 658 tests
(149 of them in `src/app.rs` alone). `src/app.rs` is 8,466 lines and holds a
single 4,176-line `impl App` block; `src/daemon.rs` is 4,645 lines.

The audit ran read-only against the live checkout, plus a hermetic re-run of
the failing suite and live measurement of the running daemon and the real
ledger on this machine. No project file was modified.

---

## Findings

### A-1 · HIGH · The supply-chain gate is red — CI and release are blocked

`cargo deny check` fails:

```
error[unmaintained]: `zbus_names` is unmaintained   RUSTSEC-2026-0371
error[unmaintained]: `zvariant` is unmaintained       RUSTSEC-2026-0370
advisories FAILED, bans ok, licenses ok, sources ok
```

`deny.toml:13` sets `unmaintained = "all"`. `zbus 5.19.0` pulls in
`zbus_names 4.3.4` and `zvariant 5.15.0`, both folded into `zbus` 6.0.0 and
both now carrying advisories. CI runs `cargo-deny-action` with
`command: check` (`.github/workflows/ci.yml:31`) and `release.yml` runs the
same gate, so **every push and every tag is red until this is resolved**.

The upgrade surface is small: `src/tray.rs` uses `zbus` in 15 places — one
`#[zbus::interface]` proc-macro attribute (`src/tray.rs:913`) and one
`zbus::blocking::Proxy::new` (`src/tray.rs:1088`). Everything else is the
tray's own logic.

**Fix direction:** bump `zbus` to `6.0` in `Cargo.toml` and adapt the two
call sites; if the 6.x API break is judged too disruptive, add a scoped
`[advisories] ignore` with a comment and a tracking issue — but the
upgrade is the honest fix.

---

### A-2 · HIGH · Two test suites are not hermetic: the MCP test is flaky against the operator's live agents, and both suites write into the real ledger

`tests/mcp.rs` isolates `TERHUB_RUNTIME_DIR` (`:132-138`) but **not** the
tmux socket and **not** the data dir. `tests/web.rs` has the same gap: it
sets `TERHUB_RUNTIME_DIR` but not the socket or the data dir, and it also
spawns a real `terhubd` (`:158`). Two consequences, both observed live.

**The MCP test's outcome depends on the operator's live agent state.** The
suite spawns a real `terhubd` (`:184`) that polls tmux — the operator's live
server — and classifies the operator's live panes. The test then waits for
an agent in state `error` on the assumption that none exists. It failed
exactly that way during this audit:

```
---- mcp_conversation_without_and_with_daemon stdout ----
panicked at tests/mcp.rs:240:
unexpected wait outcome: {"id":8,...,"text":"%2 pi error"}
```

A few minutes later the *same* suite passed (27.6 s — the full 20 s wait
ran to completion, meaning no live agent happened to be in `error` at that
moment). So the suite is **flaky, not deterministically broken**, and the
flakiness is keyed to the developer's live workload:

| Run | Result | Why |
|---|---|---|
| 13:49, full `cargo test` | **FAILED** (11.5 s) | a live `pi` agent was in `error`; the wait returned immediately |
| 13:52, hermetic re-run | **passed** (23.3 s) | `TERHUB_TMUX_SOCKET` + `TERHUB_DATA_DIR` pointed at throwaway paths |
| 13:55, full `cargo test --no-fail-fast` | **passed** (27.6 s) | no live agent was in `error` at that instant |

The hermetic re-run is the proof of cause. **CI will be green forever** —
it has no live agents — while a developer whose agents are busy will hit a
red suite at random.

**Both suites write into the user's real ledger.** The MCP test pushes a
hook event for pane `%424242` / agent `codex` / summary `"mcp probe"`
(`tests/mcp.rs:194-206`) into whatever ledger the daemon owns; the web test
does the same with `"web probe"`. Because `TERHUB_DATA_DIR` is unset, that
is the live ledger. It now holds:

```
309  "summary":"mcp probe"      (tests/mcp.rs)
333  "summary":"web probe"      (tests/web.rs)
```

**642 synthetic agent transitions in the operator's real activity history**,
plus their share of the 228,408-line / 34 MB file. Every `cargo test` run
adds more — the count moved from 307 to 309 during this audit's two runs.

`tests/backend.rs:22` and `tests/daemon.rs:83` get this right — they set
`SOCKET_ENV` themselves. The two suites that miss it are the two that spawn
a daemon *and* push hook events.

**Fix direction:** in both suites, set `SOCKET_ENV` to a per-process private
socket and `TERHUB_DATA_DIR` to a temp dir alongside the existing runtime
dir, exactly as `tests/daemon.rs:74-86` already does. A helper shared by
the two would prevent the next suite from re-introducing it.

---

### A-3 · HIGH · The test suite leaks `terhubd` processes that run forever

`tests/mcp.rs` shuts its daemon down at `:273-274` — **after** the
assertions. When an assertion fails (as it does today) the panic unwinds
past the shutdown, and the daemon is orphaned. `terhubd` has no TTL, no
idle timeout and no parent-death signal; it runs until it is killed.

Live on this machine right now: **8 orphaned daemons consuming 32 % of one
core**, several days old:

```
pid     elapsed   TERHUB_RUNTIME_DIR
489104  1-20:18    /tmp/terhub-mcp-489055
694778  2-15:07    /tmp/terhubd-flag-694674      (tests/daemon_runtime_dir.rs)
2188991 1-21:53    /tmp/terhubd-env-2188986      (tests/daemon_runtime_dir.rs)
3757286 1-18:49    /tmp/terhub-mcp-3757273
3766815 1-18:48    /tmp/terhub-mcp-3766799
3778191 1-18:48    /tmp/terhub-mcp-3778126
```

The matching `/tmp/terhubd-*` and `/tmp/terhub-mcp-*` directories are still
on disk. Three of the six test daemons are from `tests/mcp.rs` — one per
failing run.

**Fix direction:** move the daemon shutdown into a `Drop` guard so it runs
on panic as well as on success, and have the guard reap the process group.
Separately, give `terhubd` an opt-in idle TTL (`TERHUBD_IDLE_TTL_SECS`) so
an orphan cannot outlive its parent indefinitely.

---

### A-4 · HIGH · Every ledger query reads and parses the whole file, and the file is uncapped

`history::read_entries` (`src/history.rs:182-192`) does
`std::fs::read(path)` → split → `String::from_utf8_lossy` per line →
`serde_json::from_str` into an `Entry` per line. There is no size cap
anywhere (`grep max_size|MAX_LEDGER` → nothing) and the `since` filter is
applied *after* the full parse.

Measured on this machine's real 34 MB / 228,408-line ledger:

| Command | Time |
|---|---|
| `terhub status` (no ledger) | 14 ms |
| `terhub timeline --since all --limit 5` | 7 ms |
| `terhub recap --since all --limit 5` | 1,684 ms |
| `terhub activity --since today` | 1,505 ms |
| `terhub stats --since today` | 4,239 ms |
| `terhub stats --since 1h` | 3,550 ms |
| `terhub stats --since all` | 5,878 ms |
| `terhub report --since all` | 6,516 ms |

`--since 1h` still costs 3.5 s because the window is irrelevant to the read.
The ledger grows ~1.95 MB/day on this machine (34 MB over 17.65 days), and
`prune` keeps 90 days (`src/history.rs`), so the steady state is **~175 MB**
— projecting to roughly 18 s per `stats`/`report` call at today's parse
rate (~65k entries/s).

The same whole-file read runs **hourly inside the daemon** as the prune tick
(34 MB read + 34 MB rewritten + re-serialised), and — only when a user has
enabled an automation rule — `run_automation` re-reads it on **every 2 s
poll cycle** (`src/daemon.rs:1143`, gated by the `enabled` check at
`:1137-1139`). The shipped default rule is `enabled = false`, so the hot
path is off by default, but it is one config line away from a daemon that
re-parses 34 MB twice a second.

**Fix direction:** (a) make `read_entries` skip lines whose timestamp is
outside the requested window *before* parsing them — the `t` field is at a
fixed offset in the JSON, so a cheap prefix check is possible; (b) add a
byte-size cap to `prune` (keep the newest N MB, not just the newest N days)
so the file has a hard ceiling; (c) have the daemon keep a small in-memory
ring of recent rule-fire times instead of re-reading the ledger.

---

### A-5 · MED · `atomic_write` orphans its temp file, and 26 MB of litter is already in the data dir

`config::atomic_write` (`src/config.rs:1467-1504`) writes
`.<filename>.tmp.<pid>.<nonce>` and renames it. If the process dies between
`create` and `rename`, the temp file stays. Its own doc comment says *"a
future init/cleanup can prune it"* — that cleanup was never written.

`daemon.rs` has exactly such a sweeper for the registry
(`sweep_stale_tmps`, `src/daemon.rs:2117`), but nothing sweeps the data
dir. Live today in `~/.local/share/terhub/`:

```
16 MB  .activity.jsonl.tmp.2004007.1792248935249410330   (Oct 17)
10 MB  .activity.jsonl.tmp.574830.1792186759157283173   (Oct 16)
```

These are orphaned `prune` writes of the ledger. The ledger itself is
`0644` (`-rw-r--r--`), so the temp files inherit that too.

**Fix direction:** reuse `sweep_stale_tmps` for the ledger — sweep own-pid
tmp files older than an hour, never a live peer's — and call it from the
same hourly prune tick. The 26 MB already on disk is safe to delete once
the sweep lands.

---

### A-6 · MED · The dashboard's write endpoints have no authentication and no readonly mode

`terhub serve` binds 127.0.0.1 and applies an Origin guard
(`src/web.rs:50-67`) that explicitly **allows requests with no `Origin`
header** ("this guards browsers, not the local user"). There is no
authentication, and `/api/send` (`src/web.rs:24`) and `/api/interrupt`
(`:25`) are always live.

On a shared host any local user can therefore read full pane scrollback via
`/api/capture` and type into — or abort — the operator's agent panes.
`README.md` and `GUIDE.md` both describe `serve` without mentioning this.

This is not a new bug — the prior ledger records the residual risk as
"accepted, the documented loopback threat model" — but the mitigation was
never implemented. RESEARCH.md item 8 proposed it and it never shipped.

**Fix direction:** make the dashboard readonly by default and put
`/api/send` + `/api/interrupt` behind an explicit `--writable` flag, plus
an optional `--token` that is checked as a bearer header. Both are small,
self-contained changes to `src/web.rs:17-31`.

---

### A-7 · MED · The daemon socket has no credential check — the one still-open prior finding

`.pi-glla/audit-loop/findings.md:281` is the single `- [ ]` (unfixed) box
in the entire five-pass ledger:

> Daemon socket accepts Event/Register/Shutdown from any local peer with no
> credential check: any process reaching the socket can inject ghost agent
> rows, pin fake Manual records, or kill the daemon

The socket is now 0600 and the runtime dir 0700, so this is bounded to the
operator's own user — but any of the operator's own processes (a browser
extension, a CI job, a stray script) can inject a fake agent row, pin a
permanent `Manual` record, or shut the daemon down. `Shutdown` is the worst
of the three: it is an unauthenticated remote kill.

**Fix direction:** have the daemon generate a random token at startup, write
it to the runtime dir 0600, and require it in every `Request`. The client
side is a single `Request` enum.

---

### A-8 · LOW · Repository and documentation hygiene

- `conversation-2026-10-07-192409.txt` — a 1.2 MB chat transcript — is
  git-tracked. It is excluded from the crate bundle (`Cargo.toml` `exclude`)
  but still in the repository.
- `note.md` at the repo root contains scratch text (`muse shows waiting`).
- `GUIDE.md:934-936` claims *"so tests run on a private socket and never see
  the live server"*. That is only true if the developer exports
  `TERHUB_TMUX_SOCKET` by hand; `tests/mcp.rs` and `tests/web.rs` do not set
  it themselves, and CI does not set it. A-2 is the consequence.
- `GUIDE.md:896` describes the ledger as "90-day prune", which is true, but
  gives no size expectation — see A-4.
- `RESEARCH.md` (2026-10-01) is the de-facto backlog and has no status
  column. 12 of its 23 ideas have since shipped, so the document now reads
  as 23 open items when it is 11. See the reconciliation below.

---

## Ledger integrity — the retention path is not safe

These four are all in the same subsystem and compound: the prune that is
supposed to bound the ledger cannot report its own failure, can lose writes,
and the guardrails that read the ledger back fail open.

### A-9 · HIGH · `prune()` throws away its own write error, so retention can silently stop working

`src/history.rs:281` — `let _ = crate::config::atomic_write(path, &body);`

`prune` returns `Result`, and the daemon logs its failure
(`src/daemon.rs:819` — `eprintln!("terhubd: history prune failed ({e})")`),
but the **write** error never becomes that `Err`. Only a *serialization*
failure is mapped to `Err` (`src/history.rs:279`). So on a read-only data
dir, a full disk, or a failed rename, the prune writes nothing, returns
`Ok(count)`, and logs nothing.

**Consequence:** the 90-day retention silently stops, the ledger grows
without bound (there is no byte cap — A-4), and every `stats`/`report`
query gets slower forever — with no diagnostic anywhere. The error path
that was added for exactly this purpose is unreachable for the most likely
failure.

**Fix:** `crate::config::atomic_write(path, &body).map_err(|e| format!("prune ledger write failed: {e}"))?;`
— and treat the read failure at `src/history.rs:230` the same way instead of
returning `Ok(0)`.

### A-10 · HIGH · The prune is a read-modify-write with no lock, so entries appended during the rewrite are lost

`prune` reads the whole file (`src/history.rs:226`), re-serialises it, and
renames a new file over the old (`src/history.rs:281` →
`src/config.rs:1497`). The writer, `append_entry`
(`src/history.rs:152-161`), opens the path with `.append(true)` and writes
through that fd. There is no lock on either side.

**Trigger:** any `append_entry` landing between the prune's read and its
rename — the daemon appends a transition on every state change and a rule
fire on every automation action.

**Consequence:** the appender's fd points at the *old* inode, which the
rename unlinks. That entry is written successfully and then silently
discarded: the transition, prompt or rule-fire never appears in
`history stats/report/recap` even though the TUI showed it. The window is
the whole read + parse + serialise + write of the file — seconds on a
large ledger — so entries are lost roughly hourly, and more under load.

**Fix:** rotate rather than overwrite: rename the current ledger aside,
let writers append to a fresh inode, then merge the rotated file back under
a lock — or take an exclusive lock in both `append_entry` and `prune`.

### A-11 · MED · The ledger is world-readable, while the daemon log beside it is deliberately not

`src/history.rs:157-160` creates the ledger with a plain
`OpenOptions::new().create(true).append(true)` — no `.mode(0o600)` — and
`src/history.rs:153-155` creates the data dir with no mode either. Compare
`src/daemon.rs:236-241`, where the daemon's stderr log is created with
`.mode(0o600)` **because "daemon stderr may contain prompt text"** — and
the same prompt text is written to the ledger at `src/daemon.rs:727-733`
(capped at 500 chars).

Live on this machine:

```
-rw-r--r--  ~/.local/share/terhub/activity.jsonl     (0644)
drwxr-xr-x  ~/.local/share/terhub/                   (0755)
-rw-------  /run/user/1000/terhub/terhubd.log        (0600)
```

`config::atomic_write` (`src/config.rs:1487`) uses `File::create`, which
resets the mode to 0644 on **every prune** — so a manual `chmod 600` is
undone hourly.

**Consequence:** on any shared or multi-user machine, every local account
can read the user's agent prompts, summaries and project paths. The
protection that was applied to the log was never applied to the ledger.

**Fix:** create the data dir 0700 and the ledger 0600; have `atomic_write`
preserve the target's existing mode.

### A-12 · MED · A failed rule-fire append silently disables the cap and the cooldown

`src/daemon.rs:1151-1154` — `let _ = history::append_entry(&ledger, &Entry::rule_fire(...));`

The action has **already run** at that point (`src/daemon.rs:1147`). If the
append fails, the fire is never recorded, so the next pass sees no prior
fire: the cooldown never engages and the hourly cap never increments. The
rule fires again on the next tick — 2 s later — and on every tick after
that, for as long as the disk stays full. A `send-keys: "continue"` rule
would spam the pane.

**Fix:** keep an in-memory fire counter as the authoritative guardrail, and
treat an append failure as a reason to stop that pass.

### A-13 · MED · A ledger *read* error empties the rule-fire history, resetting every cooldown and cap

`src/history.rs:183-186` — `Err(_) => return Vec::new()`

The module doc at `src/history.rs:165-173` records that the invalid-UTF-8
case was fixed precisely because it *"emptied `read_rule_fires`, resetting
automation cooldowns/caps"*. The read-**error** path still has exactly that
effect. A temporarily unreadable ledger (EACCES after a permission change,
EIO, EMFILE) makes every rule look like it has never fired.

**Fix:** distinguish "no file yet" from "read error" and fail closed — skip
the pass — when the fire history cannot be read.

---

## Daemon and hook plumbing

### A-14 · MED · The hook client waits 1 s, but the server fsyncs and appends inline — events are dropped under load

`src/main.rs:777` — `daemon::send_event(&ev, Duration::from_secs(1))`.

On the server side, `handle_conn` runs on a tokio worker
(`src/daemon.rs:662`) and the `Event` arm does the registry save
(`mutate_and_save` → `save_registry` → `f.sync_all()` at
`src/daemon.rs:2192`) **and** the ledger append **inline**, before
responding. A slow disk (fsync 50–500 ms) or a hook burst (dozens of
agents reporting at once) means every worker is blocked in an fsync, the
accept loop stalls, the 1 s client timeout expires, and the hook event is
lost — the pane's state and prompt for that turn are gone with no retry.

**Fix:** move the save and the append into `spawn_blocking`, or debounce
and batch saves so N events cause one fsync.

### A-15 · MED · The pi reporter passes free-form text as an argv value — a leading `-` makes the whole hook invocation fail

`src/hooks.rs:320-322` builds
`["hook", "--agent", "pi", "--pane", pane, "--state", state, "--summary", summary]`
and calls `execFileSync`. The CLI declares plain options with **no**
`allow_hyphen_values` anywhere in `src/main.rs`.

**Trigger:** a question, prompt or UI title whose text starts with `-`
(e.g. `"- shall I push?"`).

**Consequence:** clap exits 2 on the entire invocation, the extension's
`catch {}` (`src/hooks.rs:323-325`) swallows it, and the "waiting" event
is lost — the pane keeps reading "working" while the agent sits at a
question menu. That is exactly the false negative the question handlers
were added to fix. The Claude path is unaffected: it uses
`--summary-stdin` / `--prompt-stdin`.

**Fix:** send the free text over stdin, as the Claude path already does.

### A-16 · MED · `terhub hooks install` rewrites the user's whole `settings.json` with alphabetically sorted keys

`src/hooks.rs:239-243` re-serialises the entire document with
`serde_json::to_string_pretty`. `serde_json` is declared without the
`preserve_order` feature (`Cargo.toml:55`), so `Value::Object` is a
`BTreeMap` and the user's keys come back **sorted**.

**Consequence:** the user's real Claude config returns reordered and
reformatted — a large spurious diff — and duplicate keys are collapsed
(last one wins). The `.terhub-bak` copy is written but never offered as a
restore point.

**Fix:** enable `serde_json/preserve_order`, or patch the document textually
instead of re-serialising it.

### A-17 · MED · `pi_install` overwrites a foreign file and `pi_uninstall` then deletes it

`src/hooks.rs:424-425` — *"Ours but stale, or foreign: back up once, then
overwrite."* — followed by `atomic_write(path, PI_EXTENSION)`. The module's
own contract (`src/hooks.rs:3-4`) says *"Installers only ADD our entries …
uninstall only removes entries carrying our marker, never user hooks."*

**Consequence:** a user who already has their own
`~/.pi/agent/extensions/terhub.ts` has it backed up and replaced by
`terhub hooks install`, and then deleted by `terhub hooks uninstall`. The
content survives only in `terhub.ts.terhub-bak` and must be restored by
hand.

**Fix:** refuse to overwrite a foreign file without an explicit `--force`,
and restore the backup on uninstall.

### A-18 · MED · `screen` is the only backend with no timeout, and it swallows failures

`src/screen.rs:26-36` — `Command::new(&bin).args(args).output()` with no
deadline. Compare `src/zellij.rs:7` and `src/tmux.rs:247`, which both set a
5 s timeout with the comment *"a wedged <server> surfaces as a timeout error
instead of freezing the caller"*.

**Two consequences.** (a) `terhub screen list` never returns against an
unresponsive screen server. (b) `screen::run` returns stdout unconditionally
and `list_sessions` only checks that the binary exists, so a `screen` whose
server is unreachable (`Cannot contact socket`, exit 1) yields an empty list
— the user is told they have no sessions when screen is actually broken, and
the real stderr is discarded.

**Fix:** route `screen::run` through `crate::native::run_with_timeout(cmd,
"screen -ls", 5)` and check `out.status` unless it is the documented
"No Sockets" case.

### A-19 · MED · The release workflow omits the dependency-policy and install gates that `main` enforces

`.github/workflows/release.yml:39-48` runs fmt, clippy, test and
`cargo publish --dry-run` — **no `cargo deny check`**, and no
`make check-install`. `ci.yml` has both, but `ci.yml:3-6` triggers only on
`push: branches: [main]` and `pull_request`, never on tags.

**Consequence:** tag `v0.2.4` on a commit that pulled in a crate
`deny.toml` would reject, and the release workflow is green — so the owner
publishes a dependency the project's own policy forbids. (A-1 is exactly
this gate, and it is currently red.)

**Fix:** add the same `cargo deny check` and `check-install` steps to
`release.yml`, or make `ci.yml` also run on `tags: ["v*"]`.

### A-20 · MED · The declared MSRV is never built

`Cargo.toml:5` declares `rust-version = "1.88"` and `README.md:44` says
"Rust 1.88+", but both workflows use `dtolnay/rust-toolchain@… # stable`
only. No job pins 1.88.

**Consequence:** a dependency raising its MSRV to 1.90 leaves CI green on
stable while the advertised 1.88 support silently stops working for users.

**Fix:** add an `msrv` job running `cargo +1.88 check --all-targets`.

### A-21 · LOW · The MCP state filter's error message and tool schema list 6 of the 8 real states

`src/mcp.rs:317` and the tool schema at `src/mcp.rs:268` both say
`working|waiting|error|idle|stuck|limited`. But `AgentState::parse`
(`src/agent.rs:41,44`) also accepts `done` and `unknown`, and
`README.md:105-108` documents all eight.

The filter itself **works** — it calls `AgentState::parse` — so a client
asking for `{"state":"done"}` gets data. What is wrong is the message: a
client that trips the error is *taught* that `done` and `unknown` are not
states.

**Fix:** derive the list from `AgentState::parse` and use it in both the
schema description and the error text.

### A-22 · LOW · The installed hook command exits 1 whenever the agent runs outside tmux

`src/hooks.rs:100-103` installs
`[ -n "$TMUX_PANE" ] && terhub hook --agent …`. Outside tmux the `&&`
list's status is 1, so the hook is reported as **failed** on every event
(Claude Code surfaces a non-zero, non-2 exit as a hook error) even though
there is nothing to report.

**Fix:** terminate the command with `; exit 0`.

### A-23 · LOW · `/api/capture` echoes the raw anyhow chain — the exact thing `fail()` exists to prevent

`src/web.rs:273-277` returns `{"error": format!("{e:?}")}`. The helper
`fail()` at `src/web.rs:325-335` exists precisely to prevent this — *"Don't
echo the anyhow chain (which carries tmux stderr, paths, and other local
environment detail) to any loopback client"* — and `/api/send` and
`/api/interrupt` use it. `/api/capture` does not, so a capture against no
tmux server discloses the local tmux socket path.

**Fix:** route the error arm through `fail(e)`.

### A-24 · LOW · The tray rescans all of `/proc` every 3 seconds whenever no hub is open

`src/tray.rs:1112` checks `read_hub_session()` on every 3 s poll, and
`read_hub_session` falls through to `scan_for_tui()`
(`src/tray.rs:747-756`), which reads `/proc/<pid>/cmdline` for **every**
process on the box (`src/tray.rs:667-698`).

**Consequence:** in the common tray-only deployment (tray running, TUI
closed) the tray opens one file per process, 20 times a minute, forever, to
discover a TUI that is not there.

**Fix:** rate-limit the `/proc` fallback, or run it only when the
advertisement file exists but is unusable.

### A-25 · LOW · The tray leaks an unreaped child per click and discards the spawn error

`src/tray.rs:892` — `let _ = cmd.spawn();` is the only `spawn` in the file
and nothing ever calls `wait()`. Every tray click leaves a zombie entry in
the tray's process table for as long as the tray runs, and a failed spawn
(no terminal binary, EACCES) does nothing at all, silently.

**Fix:** keep the `Child` handles and reap them in the poll loop, and log
the spawn error.

### A-26 · LOW · The dashboard re-reads and re-parses the whole ledger twice per refresh tick

`src/web.rs:192-195` and `:227-237` each do
`spawn_blocking(|| stats_rows(&h::read_entries(&h::ledger_path()), …))`, and
the dashboard polls both every 2 s (`src/web.rs:447`). With a 34 MB
ledger that is two full-file reads plus two full re-parses every 2
seconds, while the daemon writes to the same file.

**Fix:** cache the parsed entries against the file's mtime, with a short
TTL.

### A-27 · LOW · The tray documentation describes three visual features the renderer never draws

`src/tray.rs:411-419` emits exactly one path — a triangle — with no strike
line, no corner dot and no badge. But `README.md:116-118` promises "gray
+ strikethrough when the daemon is down", `GUIDE.md:589` repeats it,
`GUIDE.md:594-595` promises "a green corner dot", and `src/tray.rs:407-410`
still claims "an optional badge count is drawn centred in the triangle" —
while the test `icons_carry_no_badge_text` (`src/tray.rs:1213-1217`)
explicitly forbids the badge.

**Consequence:** a user comparing the README to the panel sees a different
icon than documented, and the in-code badge docstring would mislead the next
maintainer into "restoring" a feature the product deliberately dropped.

**Fix:** delete the badge claims from the two docstrings and correct
README/GUIDE to "one triangle per state, distinguished by colour".

### A-28 · LOW · `host doctor` throws away the ssh error

`src/fleet.rs:426-435` — `let ssh_ok = ssh_run(target, &["true"], 10).is_ok();`
then a fixed `"unreachable (check keys/host)"` detail. A rejected key
produces "unreachable (check keys/host)" plus three "skipped (no ssh)" rows
— no diagnostics at all. `ssh_run`'s non-zero-exit error also omits the host
name while its timeout error includes it (`src/fleet.rs:223-238`).

**Fix:** keep the error string and put it in `detail`; prefix the
non-zero-exit arm with `ssh {target}: `.

### A-29 · LOW · Crash-orphaned temp files are never cleaned in three places

`src/config.rs:1463-1485` (the `.{fname}.tmp.{pid}.{nonce}` write),
`src/tray.rs:729-739` (`hub-session.tmp-<pid>`) and `src/native.rs:379-386`
(`/tmp/terhub-kwin` and its `focus-<pid>-<nonce>.js` files). All three
carry a comment deferring cleanup to "a future init/cleanup" that does not
exist. A-5 is the measured instance of this in the data dir.

**Fix:** sweep `.*.tmp.*` / `*.tmp-<pid>` in the config and runtime dirs at
startup, and remove the kwin temp dir when it is empty.

### A-30 · LOW · The timeline API returns oldest-first while the guide says "newest-first"

`GUIDE.md:715` promises `GET /api/timeline?...` is "newest-first
windowing". `src/web.rs:100-105` keeps the *tail* (the newest N) but in
oldest-first order — the dashboard reverses it client-side at
`src/web.rs:419`. A script that follows the guide and takes `.[0]` gets the
oldest of the last five.

**Fix:** reverse in the handler, or change the guide to "the newest N,
oldest-first".

---

## TUI — a crash the user can reach in four keystrokes

### A-31 · HIGH · Backspace in the "new session" prompt panics the TUI

The new-session prompt is the only prompt opener that does not reset the
text cursor:

| prompt opener | resets `cursor`? |
|---|---|
| `src/app.rs:2437` (filter) | yes |
| `src/app.rs:3583` | yes |
| `src/app.rs:4243` | yes |
| `src/app.rs:4552` | yes |
| **`src/app.rs:4536-4539` (`do_new_session`)** | **no** |

```rust
fn do_new_session(&mut self) {
    self.mode = Mode::PromptNew;
    self.input.clear();          // input cleared …
}                                // … but self.cursor is left stale
```

`cancel_modal` (`src/app.rs:1980-1983`) has the same omission — it clears
`self.input` and leaves `self.cursor` where it was — so **every cancelled
prompt** leaves a stale cursor behind.

`delete_before_cursor` (`src/app.rs:2555-2562`) then does:

```rust
if self.cursor == 0 { return; }
self.cursor -= 1;
let at = self.cursor_offset();   // empty input → unwrap_or(len) → 0
self.input.remove(at);           // String::remove(0) on "" → PANIC
```

**Trigger (four keystrokes):** press `f`, type `npm` (cursor = 3), press
`Enter` — the filter prompt's Enter arm calls `cancel_modal()`
(`src/app.rs:2498`), clearing the input but leaving the cursor at 3. Press
`n` to open the new-session prompt (`do_new_session` clears the input, the
cursor is still 3). Press `Backspace` → `self.input.remove(0)` on an empty
`String` → `panic: cannot remove a char from the end of an empty string`.

**Consequence:** the TUI process aborts. There is no `catch_unwind` or
`panic::set_hook` anywhere in `src/main.rs` or `src/app.rs`, so nothing
recovers the process; the terminal is restored by the guard, but the hub
is gone and any unsaved view state is lost. The same crash is reachable
after **any** prompt cancelled with `Esc`.

Note the adjacent `delete_at_cursor` (`src/app.rs:2564-2568`) is safe — it
checks `at < self.input.len()` first. Only the Backspace path is broken.

**Fix:** set `self.cursor = 0` in `do_new_session` *and* in `cancel_modal`,
and make `delete_before_cursor` a no-op on an empty input. A regression test
should drive the four-keystroke sequence.

### A-32 · MED · The sparkline's 64 KB tail slice can land mid-character

`src/app.rs:1061-1064`:

```rust
let cap = screen.len().min(65536);
self.sparkline_prev.insert(id.to_string(), screen[screen.len() - cap..].to_string());
```

`screen.len()` is a **byte** count, so `screen.len() - cap` can fall inside
a UTF-8 sequence. The codebase already knows the idiom — `truncate_msg`
(`src/config.rs:1555-1561`) floors to a char boundary for exactly this
reason ("`String::truncate` panics mid-multibyte-char, which would turn a
config error report into a crash").

**Trigger:** `[ui] activity_sparkline = true`, a native-tab capture larger
than 64 KB, and a multi-byte character straddling the cut — routine, since
captures are full of box-drawing and `✓`.

**Consequence:** `panic: byte index N is not a char boundary` on startup or
on `F5`.

**Fix:** floor the cut to a char boundary before slicing, exactly as
`truncate_msg` does.

### A-33 · MED · The 3 s tick re-captures the preview synchronously — up to a 10 s SSH timeout on the UI thread

`src/app.rs:1336-1341` (the tick) → `follow_preview`
(`src/app.rs:1854-1863`) → `capture_preview` (`src/app.rs:1746-1769`) →
`capture_remote_preview` (`src/app.rs:1637-1664`) →
`fleet::remote_capture` → `ssh_run(target, &[&cmd], 10)`
(`src/fleet.rs:368-371`).

**Trigger:** the preview is open on a fleet row and the host is slow or
unreachable.

**Consequence:** the UI thread blocks inside the tick for the full 10 s SSH
timeout — no key handling, no redraw — and then does it again every 3 s.
For a local row the same tick spawns two `tmux` processes
(`capture-pane` + `display`) plus, every 30 s, three `git` processes, all
synchronously on the UI thread.

**Fix:** re-capture on a TTL rather than every tick, and move the capture
(especially the SSH one) off the UI thread or behind a short connect
timeout.

### A-34 · MED · Three of the four "clear the search" paths leave the filter chip naming a filter that is hiding nothing

The header chip is rendered from `active_saved_filter` **alone**
(`src/ui.rs:505-511`), never from `app.filter`. Only
`clear_filter` (`src/app.rs:3319-3328`) clears it. These three paths clear
or restore `self.filter` without touching the name:

- `src/app.rs:2768-2773` — `Esc` in the table
- `src/app.rs:2442-2443` — the modifier-qualified `filter_clear`
- `src/app.rs:2450-2455` — `Esc` in the prompt (restores the pre-edit query)

**Consequence:** apply a saved filter, then press `Esc` in the table — the
table shows every row but the chip still reads `◼ filter: <name>`. That
contradicts `GUIDE.md:163-165` ("a label naming every filter that is hiding
rows") and `GUIDE.md:188-189`.

**Fix:** clear/restore `active_saved_filter` wherever `self.filter` is
cleared or restored, or derive the chip from both so they cannot disagree.

### A-35 · MED · Deleting the last saved filter wedges the Settings popup

`delete_saved_filter` (`src/app.rs:3268-3278`) and the rename-onto-existing
merge (`src/app.rs:3359-3362`) both `remove` an entry without adjusting
`settings_selected` (an item *ordinal*, per `settings_item_rows` at
`src/app.rs:3409-3417`) or `settings_scroll` (a *row* index). The renderer
(`src/ui.rs:1610-1618`) matches on the ordinal, so when no item has that
ordinal nothing is highlighted and the scroll is not corrected either.

**Trigger:** Settings → Filters, select the **last** saved filter, press
`d`.

**Consequence:** no row is highlighted, `Enter` does nothing
(`src/app.rs:4170-4174` — `items.get(self.settings_selected)` is `None`),
and `↓` cannot move because the ordinal is out of range
(`src/app.rs:4094-4102`) — only `↑` recovers.

**Fix:** clamp `settings_selected` to `settings_item_rows().len() - 1` and
re-derive `settings_scroll` after a removal or merge.

### A-36 · LOW · The mouse-hover highlight is wiped by every 3 s tick

`refresh_filtered` clears `self.hover` (`src/app.rs:1452-1453`) and runs on
every tick through `merge_caches` → `refresh`. The only writer is
`MouseEventKind::Moved` (`src/main.rs:2398-2405`).

**Consequence:** park the mouse over a row and the bold hover highlight
disappears within 3 s, returning only when the mouse moves again — the row
under a stationary pointer stops looking hovered.

**Fix:** do not clear `hover` in `refresh_filtered`, or re-derive it from
the last mouse position.

---

## RESEARCH.md reconciliation

`RESEARCH.md` is three weeks stale. Status of each idea, verified against the
code at HEAD:

| # | Idea | Status | Evidence |
|---|---|---|---|
| 1 | Quota is telemetry, not an error | **shipped** | distinct `AgentState::Limited` (`src/ui.rs:797`) |
| 2 | Timeouts as config | **shipped** | `[daemon] poll_secs/stuck_secs/stale_secs/quiet_idle_secs/waiting_idle_secs` (`src/config.rs:914-927`) |
| 3 | Hook spool + backfill | **not shipped** | no `spool` anywhere in `src/` |
| 4 | `init` that wires agent hooks | **shipped** | `cmd_init` calls `cmd_hooks(Install{agent:"all"})` (`src/main.rs`) |
| 5 | Stats/report upgrades | **partial** | `--group-by/--json/--markdown/--sort/--view heatmap` ship; `--graph`, `--verbose`, `--exclude` do not |
| 6 | Turn durations | **shipped** | `last_turn_secs` rendered in the Turn column (`src/ui.rs:1216`) |
| 7 | Read agent session logs | **not shipped** | no rollout/JSONL reader |
| 8 | Serve hardening | **not shipped** | see A-6 |
| 9 | Per-pane web deep links | **not shipped** | no `?pane=` routing |
| 10 | Field-scoped filters, command-log drawer | **not shipped** | — |
| 11 | `:` command bar, movable columns, side pane, theme dir, tree view | **not shipped** | sparkline and custom commands *did* ship |
| 12 | Project layer (templates, worktrees) | **partial** | only `[[template]]` message snippets (`src/config.rs:786`); no session templates |
| 13 | Snapshot/restore workspace | **not shipped** | `snapshot_rows` is fleet rows, not a workspace snapshot |
| 14 | Agent launcher | **partial** | named spawn profiles ship (`src/config.rs:754`); worktrees do not |
| 15 | Arbitrary process rows | **shipped** | `register` / `unregister` |
| 16 | Fleet depth | **partial** | host `label` ships; selectors, `-o wide`, relay cache do not |
| 17 | Peek upgrade | **partial** | digit-list picker ships; not the muxa-style live overlay |
| 18 | Prompt history | **shipped** | `terhub recap` |
| 19 | Onboard tour | **not shipped** | — |
| 20 | Human-side stats | **shipped** | `human_focus` ledger entries (`src/history.rs:97`) |
| 21 | Agent messaging / mailbox | **not shipped** | — |
| 22 | Work model (Workspace→Work→Run) | **not shipped** | — |
| 23 | More backends (rmux, herdr) | **not shipped** | — |

**9 shipped, 6 partial, 8 not shipped.** The eight unshipped items are the
real backlog; the six partial ones are the cheapest wins.

---

## Verified healthy — do not re-audit

- `clippy --all-targets -D warnings` clean; `cargo fmt --check` clean.
- The 40-kLOC tree has no `unwrap()` on user-controlled input that I could
  reach, and the 26 `unsafe` blocks are all libc calls with a stated
  invariant (killpg, flock, geteuid, sysconf).
- `TERHUB_TMUX_SOCKET` isolation is correctly implemented in
  `src/tmux.rs:235-271` and correctly used by `tests/backend.rs` and
  `tests/daemon.rs`.
- The runtime dir's symlink guard, the daemon flock, the unique-tmp
  `save_registry`, the hourly prune tick, the per-line lossy ledger decode
  and the starttime identity check are all present and correct.
- The web dashboard's `esc()` encodes all five HTML metacharacters and the
  served bytes are asserted, not just the constant.
- `cargo package --list` is clean of dev junk.
- The crate metadata, tag workflow and `deny.toml` strictness gates are
  sound in principle — A-1 is a live advisory, not a weak gate.
