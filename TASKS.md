# terhub — improvement task list

Derived from `AUDIT.md` (2026-10-19) and the reconciliation of `RESEARCH.md`
against the code at `main` @ `680acfd` / v0.2.3.

Ordered by **unblock-first, then value-per-effort**. Each task states what
"done" means so it can be picked up without re-deriving the context.

Legend — effort: **S** (< 1 h) · **M** (1 day) · **L** (1 week) ·
impact: **blocker** / **high** / **medium** / **low**.

---

## P0 · Unblock — do these before anything else

### T-1 · Make `cargo deny` pass again — CI and release are red
**Effort M · impact blocker** · source: `AUDIT.md` A-1

`zbus 5.19.0` pulls in `zbus_names 4.3.4` (RUSTSEC-2026-0371) and
`zvariant 5.15.0` (RUSTSEC-2026-0370); `deny.toml` sets
`unmaintained = "all"`, so `cargo deny check` fails and every CI run and
every release tag is red.

- Bump `zbus` to `6.0` in `Cargo.toml`; adapt the two real call sites —
  `#[zbus::interface]` (`src/tray.rs:913`) and `zbus::blocking::Proxy::new`
  (`src/tray.rs:1088`).
- If the 6.x API break is judged too disruptive, take the scoped
  `[advisories] ignore` route **with a tracking issue and a comment naming
  the upstream issue** — but the upgrade is the honest fix.
- The tray's behaviour is unchanged by either route; `tests/tray.rs` (29
  unit tests) plus the live tray on this machine are the regression net.

**Done when** `cargo deny check` exits 0 and the tray still renders live
counts on a KDE/Plasma session.

### T-2 · Make `tests/mcp.rs` and `tests/web.rs` hermetic
**Effort S · impact blocker** · source: `AUDIT.md` A-2

Both suites set `TERHUB_RUNTIME_DIR` but neither sets the tmux socket or
the data dir. Consequences today, both observed live:

- **The MCP suite is flaky against the developer's own workload.** It
  failed this audit with `unexpected wait outcome: {"text":"%2 pi error"}` —
  a live `pi` agent the test's own daemon had classified — and passed
  minutes later with no change to the code. CI is permanently green (it has
  no live agents); a busy developer gets a random red suite.
- **Both suites write into the real ledger.** The live
  `~/.local/share/terhub/activity.jsonl` holds **309 `"mcp probe"` and 333
  `"web probe"` entries** — 642 synthetic agent transitions in the user's
  real activity history, growing with every run.

- Set `terhub::tmux::SOCKET_ENV` to a per-process private socket and
  `TERHUB_DATA_DIR` to a temp dir, following `tests/daemon.rs:74-86` exactly.
- Extract the four `set_var` lines into one shared test helper (e.g.
  `tests/common/mod.rs`) so the next suite inherits the isolation instead of
  re-deriving it.
- Kill the `tmux -L <name> kill-server` for the private socket in teardown.

**Done when** `cargo test` is green on a machine with live agents in every
state (including `error`), the live ledger contains no `"mcp probe"` or
`"web probe"` entries, and `tests/mcp.rs` passes with and without
`TERHUB_TMUX_SOCKET` set in the environment.

### T-3 · Stop the test suite leaking `terhubd` processes
**Effort S · impact high** · source: `AUDIT.md` A-3

The MCP suite shuts its daemon down *after* the assertions, so a failed
assertion orphans it. `terhubd` has no TTL and no parent-death signal, so
the orphan runs forever. **8 leaked daemons are consuming 32 % of a core on
this machine right now**, and `/tmp/terhubd-*` + `/tmp/terhub-mcp-*` dirs
remain.

- Move the shutdown into a `Drop` guard so it runs on panic as well as on
  success, and reap the process group (mirror the `try_wait`-loop pattern in
  `tests/daemon.rs:41-48`).
- Add an opt-in idle TTL to the daemon (`TERHUB_IDLE_TTL_SECS`, default off)
  so an orphan cannot outlive its parent indefinitely.
- One-time cleanup: kill the 8 orphans that predate this work.

**Done when** a deliberately failing test leaves no `terhubd` process and no
`/tmp/terhub-*` directory behind.

---

## P1 · High value, small effort

### T-4 · Make ledger reads window-aware before parsing
**Effort M · impact high** · source: `AUDIT.md` A-4

`read_entries` (`src/history.rs:182`) reads the whole file and parses every
line before the `since` filter is applied. Measured on the real 34 MB
ledger: `stats --since 1h` = 3.5 s, `stats --since all` = 5.9 s,
`report --since all` = 6.5 s — the window barely matters.

- The `t` field sits at a fixed offset in each line, so a cheap prefix scan
  can skip out-of-window lines before `serde_json` runs.
- Better still: stream the file line by line instead of
  `std::fs::read` + `Vec<String>` + a second pass, which also removes the
  ~70 MB of transient allocation per query.
- Give `read_entries` a `since` parameter so every call site
  (`src/main.rs:1252,1269,1290,1336,1403,1435`; `src/web.rs:193,229`) can
  push the filter down.

**Done when** `terhub stats --since 1h` on a 34 MB ledger returns in
well under 500 ms, and `stats --since all` still returns identical numbers.

### T-5 · Give the ledger a byte ceiling, not just a 90-day window
**Effort S · impact high** · source: `AUDIT.md` A-4

The ledger grows ~1.95 MB/day here and `prune` only bounds it by age, so the
steady state is ~175 MB — and every query pays for all of it. There is no
size cap anywhere in the codebase.

- Add a `max_bytes` to `prune` (default e.g. 64 MB) that additionally drops
  the oldest entries past the byte budget, and surface both limits in
  `terhub history status`.
- Consider a rolling daily shard (`activity.jsonl` → `activity/2026-10-19.jsonl`)
  so a single file never has to be parsed whole.

**Done when** the ledger's on-disk size stays under the configured ceiling
on a busy machine, and the ceiling is visible in `history status`.

### T-6 · Sweep orphaned `atomic_write` temp files
**Effort S · impact medium** · source: `AUDIT.md` A-5

`config::atomic_write` (`src/config.rs:1467`) leaves
`.<file>.tmp.<pid>.<nonce>` behind if the process dies mid-write. Its doc
comment says "a future init/cleanup can prune it" — that cleanup was never
written. **16 MB and 10 MB orphans are sitting in `~/.local/share/terhub/`
right now.** `daemon.rs` already has the right primitive
(`sweep_stale_tmps`, `src/daemon.rs:2117`) but it only runs for the
registry.

- Reuse `sweep_stale_tmps` for the data dir: remove own-pid tmp files older
  than an hour, never a live peer's.
- Call it from the hourly prune tick.
- Delete the two existing orphans once the sweep ships.

**Done when** the data dir holds no `.*.tmp.*` file older than an hour after
a prune cycle, and a crash-injected write leaves nothing behind after it.

### T-7 · Authenticate the daemon socket
**Effort M · impact medium** · source: `AUDIT.md` A-7

This is the **only unfixed `- [ ]` box in the entire five-pass audit
ledger** (`.pi-glla/audit-loop/findings.md:281`). Any process running as
the operator can inject ghost agent rows, pin permanent `Manual` records, or
send `Shutdown` and kill the daemon.

- Generate a random token at daemon start, write it 0600 into the runtime
  dir, and require it in every `Request` (one enum, one helper).
- The client side is the same change in `daemon::send_request`.
- Keep the runtime dir 0700 and the socket 0600 as they already are.

**Done when** a request without the token is rejected, and a regression
test proves a socket peer cannot inject a row or shut the daemon down.

### T-8 · Make the dashboard readonly by default
**Effort M · impact medium** · source: `AUDIT.md` A-6, RESEARCH.md #8

`/api/send` and `/api/interrupt` are always live with no auth; the Origin
guard explicitly allows no-`Origin` requests. On a shared host any local
user can drive the operator's panes.

- `terhub serve` becomes readonly by default; `--writable` enables the two
  write routes.
- Add `--token`, checked as a bearer header, and have the embedded
  dashboard read it from the URL it was opened with.
- Document the model in `README.md` and `GUIDE.md` (neither mentions auth
  today).

**Done when** `terhub serve` refuses `POST /api/send` without `--writable`,
and a wrong token is rejected.

### T-9 · Make the ledger retention path honest and lossless
**Effort M · impact high** · source: `AUDIT.md` A-9, A-10

Two defects in the same function, and they compound.

`prune()` throws away its own write error (`src/history.rs:281` —
`let _ = crate::config::atomic_write(path, &body);`), so on a read-only data
dir or a full disk the prune silently does nothing, forever, while the
daemon's `history prune failed` log line can never fire for that case.
And `prune` is an unlocked read-modify-write: `append_entry` writes through
an fd on the old inode that the rename unlinks, so every transition or
rule-fire appended during the rewrite is **written successfully and then
discarded**.

- Map the `atomic_write` result to `Err` (and the read failure at
  `src/history.rs:230` the same way) so the daemon's existing log line
  becomes reachable.
- Make the prune append-safe: rotate the current ledger aside, let writers
  append to a fresh inode, then merge the rotated file back — or take an
  exclusive lock in both `append_entry` and `prune`.

**Done when** a read-only data dir produces a visible "prune failed" in
`terhubd.log`, and an entry appended during a prune is still present in
`history activity` afterwards.

### T-10 · Make the automation guardrails fail closed
**Effort S · impact medium** · source: `AUDIT.md` A-12, A-13

Both directions of the guardrail state fail **open**:

- A failed fire-append (`src/daemon.rs:1151` — `let _ = history::append_entry(...)`)
  means the fire is never recorded, so the cooldown never engages and the
  cap never increments — the rule re-fires every 2 s until the disk frees
  up.
- A ledger *read* error (`src/history.rs:183-186` — `Err(_) => return Vec::new()`)
  makes every rule look like it has never fired. The module's own doc
  records that the invalid-UTF-8 case was fixed for exactly this reason;
  the read-error path was missed.

- Keep an in-memory fire counter as the authoritative guardrail, seeded from
  the ledger at startup only.
- Distinguish "no file yet" from "read error" and skip the pass on error.

**Done when** a rule with `cooldown_secs = 3600` fires at most once per
hour even while the ledger is unwritable, and a regression test proves it.

### T-11 · Lock down the ledger's permissions
**Effort S · impact medium** · source: `AUDIT.md` A-11

The ledger is created 0644 in a 0755 data dir, while the daemon log beside
it is deliberately 0600 *because it may contain prompt text* — and the
ledger holds the same prompt text. `atomic_write` resets the mode to 0644
on every prune, so a manual `chmod 600` is undone hourly.

- Create the data dir 0700 and the ledger 0600 (`.mode(0o600)`).
- Make `config::atomic_write` preserve the target file's existing mode
  instead of `File::create`'s 0644.

**Done when** `~/.local/share/terhub/activity.jsonl` is 0600 in a 0700 dir
and stays that way across a prune.

### T-12 · Move the hook event path off the async runtime
**Effort M · impact medium** · source: `AUDIT.md` A-14

The hook client waits 1 s (`src/main.rs:777`). The server's `Event` arm
does the registry save — including an `fsync` — and the ledger append
**inline on a tokio worker** before responding. A slow disk or a hook
burst blocks every worker, the accept loop stalls, the 1 s timeout
expires, and the event is lost with no retry.

- Move the save and the append into `spawn_blocking`, or debounce and batch
  saves so N events cause one fsync.

**Done when** a burst of 200 hook events is fully delivered with the
ledger on a deliberately slow filesystem.

### T-13 · Fix the three hook-install defects
**Effort M · impact medium** · source: `AUDIT.md` A-15, A-16, A-17

- **The pi reporter's argv breaks on a leading `-`.** `src/hooks.rs:320`
  passes free-form text as `--summary <value>`; the CLI has no
  `allow_hyphen_values`, so a question like `"- shall I push?"` makes clap
  exit 2 and the "waiting" event is lost. The Claude path already uses
  `--summary-stdin`; the pi path should too.
- **Installing rewrites the user's whole `settings.json` with sorted keys.**
  `serde_json` is declared without `preserve_order` (`Cargo.toml:55`), so
  the re-serialised document comes back alphabetically reordered and
  duplicate keys collapsed. Enable the feature, or patch the text.
- **`pi_install` overwrites a foreign file** and `pi_uninstall` then deletes
  it, contradicting the module's own contract at `src/hooks.rs:3-4`. Refuse
  without `--force`, and restore the backup on uninstall.

**Done when** a summary starting with `-` reaches the ledger, the user's
`settings.json` comes back byte-identical apart from the added hook, and
`hooks install` refuses a foreign extension file.

### T-14 · Give `screen` a timeout and stop hiding its failures
**Effort S · impact medium** · source: `AUDIT.md` A-18

`screen::run` (`src/screen.rs:26`) is the only backend call with no
deadline — `zellij` and `tmux` both use 5 s — and it returns stdout
unconditionally, so a broken screen server reads as "no sessions" with the
real stderr discarded.

- Route it through `crate::native::run_with_timeout(cmd, "screen -ls", 5)`.
- Check `out.status` unless it is the documented "No Sockets" case, and
  surface stderr otherwise.

**Done when** `terhub screen list` against an unresponsive screen server
returns a timeout error instead of hanging, and a broken server says so
instead of "no sessions".

### T-15 · Close the release-workflow gaps
**Effort S · impact high** · source: `AUDIT.md` A-19, A-20

- `release.yml` runs fmt/clippy/test/publish-dry-run but **not**
  `cargo deny check` and not `check-install`, which `ci.yml` does run —
  and `ci.yml` never triggers on tags. A tag on a commit carrying a
  dependency `deny.toml` would reject would publish anyway. (A-1 is
  precisely this gate, red today.)
- The declared MSRV `1.88` (`Cargo.toml:5`, `README.md:44`) is never built;
  both workflows use `stable`. Add an `msrv` job running
  `cargo +1.88 check --all-targets`.

**Done when** a tag run executes the same gates as a `main` run, and the
MSRV job fails when a dependency raises its minimum Rust version.

---

### T-16 · Fix the new-session prompt panic
**Effort S · impact high** · source: `AUDIT.md` A-31

Four keystrokes abort the TUI: `f`, type `npm`, `Enter`, then `n`,
`Backspace`. `do_new_session` (`src/app.rs:4536`) clears the input but
leaves `self.cursor` at 3; `delete_before_cursor` then computes
`cursor_offset() == 0` on the empty string and calls `String::remove(0)` →
`panic: cannot remove a char from the end of an empty string`. There is no
`catch_unwind` anywhere in the TUI, so the process dies.

`cancel_modal` (`src/app.rs:1980`) has the same omission, so **any**
prompt cancelled with `Esc` leaves a stale cursor and arms the same crash.

- Set `self.cursor = 0` in `do_new_session` and in `cancel_modal` — the
  other four prompt openers (`src/app.rs:2437, 3583, 4243, 4552`) already
  do this.
- Make `delete_before_cursor` a no-op on an empty input, matching the
  guard `delete_at_cursor` already has.
- Add a regression test that drives the exact keystroke sequence.

**Done when** the four-keystroke sequence leaves the TUI running, and the
same after cancelling any prompt with `Esc`.

### T-17 · Fix the sparkline's mid-character slice
**Effort S · impact medium** · source: `AUDIT.md` A-32

`src/app.rs:1061-1064` slices `screen[screen.len() - cap..]` at a byte
offset that can fall inside a UTF-8 sequence → `panic: byte index N is not
a char boundary` on startup or `F5` with `activity_sparkline = true`.

Floor the cut to a char boundary, exactly as `truncate_msg`
(`src/config.rs:1555-1561`) already does.

**Done when** a capture with a multi-byte character straddling the 64 KB
cut no longer panics.

### T-18 · Stop the tick from blocking the UI thread
**Effort M · impact medium** · source: `AUDIT.md` A-33, A-34, A-35, A-36

Four TUI issues in the same area:

- **A-33** — the 3 s tick re-captures the preview synchronously; on a
  fleet row that is a 10 s SSH timeout **on the UI thread**, freezing the
  TUI, and it repeats every 3 s. Re-capture on a TTL and move the capture
  off the UI thread.
- **A-34** — three of the four "clear the search" paths clear `self.filter`
  without clearing `active_saved_filter`, so the header chip keeps naming a
  filter that is hiding nothing (`src/app.rs:2768, 2442, 2450` vs
  `src/app.rs:3319`).
- **A-35** — deleting the last saved filter leaves `settings_selected`
  pointing at a non-existent ordinal, so the Settings popup highlights
  nothing, `Enter` does nothing and `↓` cannot move
  (`src/app.rs:3268`, `:3359`).
- **A-36** — every 3 s tick wipes the mouse-hover highlight
  (`src/app.rs:1452`), so a stationary pointer stops looking hovered.

**Done when** the preview does not block the UI thread for more than one
frame, the chip and the table always agree, deleting the last saved filter
keeps the popup navigable, and the hover highlight survives a tick.

---

## P2 · Product features — the cheap half of the RESEARCH backlog

These are the "partial" rows in the reconciliation table: the expensive half
already shipped, so each is a bounded increment.

| # | Task | Effort | Impact | Note |
|---|---|---|---|---|
| T-19 | `stats --graph` (WACT over time) + `--verbose` diagnostics | M | medium | RESEARCH #5; `--group-by/--json/--markdown/--view heatmap` already ship |
| T-20 | Per-pane web deep links (`?pane=%69`) | S | medium | RESEARCH #9; the dashboard already renders a pane list |
| T-21 | Field-scoped filters (`state:waiting`, `agent:pi`) | M | high | RESEARCH #10; the tokenizer already exists for the live search |
| T-22 | `:` command bar | M | medium | RESEARCH #11 |
| T-23 | Movable / hideable columns | L | medium | RESEARCH #11; a table of `ColumnSpec` |
| T-24 | Fleet selectors + `-o wide` output | M | medium | RESEARCH #16; host `label` already ships |
| T-25 | Peek upgrade — live overlay with last prompt/response | L | high | RESEARCH #17; the digit-list picker is the current shape |
| T-26 | Session/project templates (YAML: windows, panes, layout, commands) | L | high | RESEARCH #12; only `[[template]]` snippets exist today |
| T-27 | Hook spool + backfill on daemon restart | M | high | RESEARCH #3; nothing matches `spool` in the tree. A daemon restart currently goes blind for `stale_secs` |
| T-28 | Read agent session logs (Claude JSONL / Codex rollout) | L | medium | RESEARCH #7; a 4th signal source independent of hooks |
| T-29 | `stats --exclude-pane` / `--exclude-session` glob scopes | S | low | RESEARCH #5 |

**T-21 and T-27 are the two worth doing first.** T-21 is the feature the
table most obviously lacks (the current search is a plain substring/OR token
match over free text), and T-27 removes a real gap: if the daemon dies, the
hooks that fired during the outage are lost and the pane stays blind until
`stale_secs` lapses.

---

## P3 · A short sweep of the remaining low findings

### T-30 · Clear the low-severity backlog in one pass
**Effort M · impact low each, but they add up**

Each is small and independently verifiable. Pick them off together:

- **A-21** — the MCP state filter's error text and tool schema list 6 of
  the 8 states `AgentState::parse` accepts (`src/mcp.rs:268,317`); derive
  the list from the enum.
- **A-22** — the installed hook command exits 1 outside tmux
  (`src/hooks.rs:100`); terminate it with `; exit 0`.
- **A-23** — `/api/capture` echoes the anyhow chain while `fail()` exists
  to prevent exactly that (`src/web.rs:273` vs `:325`); route it through
  `fail(e)`.
- **A-24** — the tray rescans every `/proc` entry every 3 s when no hub is
  open (`src/tray.rs:1112`, `:667`); rate-limit the fallback.
- **A-25** — the tray leaks an unreaped child per click and discards the
  spawn error (`src/tray.rs:892`); reap the children, log the error.
- **A-26** — the dashboard re-parses the whole ledger twice per 2 s tick
  (`src/web.rs:192,227`); cache against the file's mtime. This one also
  pays off T-4.
- **A-27** — the tray docs promise a badge and a strikethrough that the
  renderer never draws (`src/tray.rs:407-419` vs `README.md:116`,
  `GUIDE.md:589`); delete the claims from the code and correct the docs.
- **A-28** — `host doctor` discards the ssh error and `ssh_run`'s
  non-zero-exit arm omits the host (`src/fleet.rs:426`, `:223`); keep the
  error and prefix it with the target.
- **A-29** — crash-orphaned temp files in three places
  (`src/config.rs:1463`, `src/tray.rs:729`, `src/native.rs:379`), all with
  a comment deferring cleanup to a "future init/cleanup" that does not
  exist; sweep them at startup.
- **A-30** — the timeline API returns oldest-first while `GUIDE.md:715`
  promises "newest-first" (`src/web.rs:100`); reverse in the handler or
  fix the guide.
- Also: an MCP JSON-RPC **batch** is handled serially in one thread
  (`src/mcp.rs:27-42`), so a `terhub_wait` inside a batch wedges the rest
  of that batch — JSON-RPC 2.0 says to process batch elements in parallel,
  and no test covers the batch path.

**Done when** each item has a regression test or a doc fix, and the
`AUDIT.md` boxes for A-21…A-30 are checked.

---

## P4 · Bigger bets — decide deliberately

| # | Task | Effort | Impact | Note |
|---|---|---|---|---|
| T-31 | Onboard tour on a throwaway tmux server | M | high | RESEARCH #19; the highest-leverage adoption tool in the whole research set. Nothing wires the hooks automatically today except `terhub init` |
| T-32 | Snapshot / restore workspace (sessions, layout, cwd) | L | medium | RESEARCH #13 |
| T-33 | Per-task git worktrees | L | medium | RESEARCH #12/#14 |
| T-34 | More backends (rmux, herdr) | L | low | RESEARCH #23; the parity matrix in `GUIDE.md:827` is where the answer gets recorded |
| T-35 | Agent messaging / mailbox, work model | XL | low | RESEARCH #21/#22; a second product, not an increment |

---

## P5 · Structural maintenance

### T-36 · Split `src/app.rs`
**Effort L · impact medium**

`src/app.rs` is 8,466 lines with a single 4,176-line `impl App` block and
350 functions. It holds the focus model, the filter engine, the key
dispatch, the settings popup, the fleet/remote merge and the preview. Every
audit pass so far has had to read the whole file to reason about a single
key.

- Split by concern: `app/focus.rs`, `app/filter.rs`, `app/keys.rs`,
  `app/settings.rs`, `app/preview.rs`, keeping `App` as the façade.
- 149 unit tests already live in the file; move them with their subject.

### T-37 · Give `RESEARCH.md` a status column
**Effort S · impact low**

`RESEARCH.md` is the de-facto backlog and now reads as 23 open items when
9 have shipped and 6 are partial. Add a `Status` column (shipped / partial /
open) and point each open row at a task ID in this file, so the two
documents cannot drift apart again.

### T-38 · Repository hygiene
**Effort S · impact low**

- `conversation-2026-10-07-192409.txt` — a 1.2 MB chat transcript — is
  git-tracked. Remove it from the tree (it is already excluded from the
  crate bundle).
- `note.md` at the repo root contains scratch text. Delete it.
- `GUIDE.md:934-936` claims "tests run on a private socket and never see the
  live server". That is only true when the developer exports
  `TERHUB_TMUX_SOCKET` by hand. T-2 makes it true unconditionally; the
  sentence should then say so.

---

## The shape of the backlog

Of the 23 ecosystem ideas in `RESEARCH.md`, **9 shipped, 6 are partial, 8
never started**. The partial six (T-19, T-20, T-22, T-24, T-25, T-26) are
the best value in the backlog: the expensive half is already paid for, and
each is a bounded increment on working code.

The audit findings are a different shape again. Nothing in the last five
passes touched **test hygiene, resource lifecycle, ledger scale, the
release gate, or the TUI's panic paths** — and that is where the live damage
was: a red CI and a red release gate, a suite whose result depends on the
developer's live agents, 8 orphaned daemons eating 32 % of a core, a 34 MB
file that makes `terhub report` take six and a half seconds, 642 synthetic
agent rows in the user's real history, a retention path that can silently
stop working, and a four-keystroke crash of the TUI itself.

The P0 and P1 work is **15 tasks, most of them under a day**, and between
them they clear the red gate, the flaky suite, the resource leak, the
ledger's growth and correctness problems, and the crash. The P2 product work
is where the visible feature gains are.
