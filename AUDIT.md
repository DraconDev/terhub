# terhub — audit 2026-10-19

A fresh audit pass over the tree at `main` @ `680acfd` (v0.2.3). Five prior
audit passes are recorded in `.pi-glla/audit-loop/findings.md` (2026-10-08 →
2026-10-17) and fixed ~120 defects. This pass does **not** re-report those;
it covers ground the earlier passes did not: **test hygiene, resource
lifecycle, ledger scale, and the state of the improvement backlog**.

## What was run

| Gate | Result |
|---|---|
| `cargo clippy --offline --all-targets -- -D warnings` | clean |
| `cargo fmt --check` | clean |
| `cargo test --offline` | **1 failure** — `tests/mcp.rs` (see A-2) |
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

### A-2 · HIGH · The MCP test is not hermetic: it fails, and it pollutes the operator's live ledger

`tests/mcp.rs` isolates `TERHUB_RUNTIME_DIR` (`:132-138`) but **not** the
tmux socket and **not** the data dir. Two consequences, both observed live:

**It fails.** The suite spawns a real `terhubd` (`:184`). That daemon polls
tmux — the operator's live server — and classifies the operator's live panes.
At the moment of the run, the live registry had a `pi` agent in `error`
state, so the test's "no agent is ever in error" assumption was false:

```
---- mcp_conversation_without_and_with_daemon stdout ----
panicked at tests/mcp.rs:240:
unexpected wait outcome: {"id":8,...,"text":"%2 pi error"}
```

Re-running the identical suite with `TERHUB_TMUX_SOCKET` and
`TERHUB_DATA_DIR` pointed at throwaway paths **passes** (1/1, 23.3 s). That
is the proof of cause, not a coincidence.

**It writes into the user's real data.** The test pushes a hook event for
pane `%424242` / agent `codex` / summary `"mcp probe"`
(`tests/mcp.rs:194-206`) into whatever ledger the daemon owns. Because
`TERHUB_DATA_DIR` is unset, that is the live ledger. It now holds **307
`%424242` entries**, the newest from today's run. Every `cargo test --test
mcp` adds another agent that never existed.

`tests/web.rs` has the same gap: it sets `TERHUB_RUNTIME_DIR` but not the
socket or the data dir, and it also spawns a real `terhubd` (`:158`).

`tests/backend.rs:22` and `tests/daemon.rs:83` get this right — they set
`SOCKET_ENV` themselves. The two suites that miss it are the two that
spawn a daemon.

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

Live on this machine right now: **9 orphaned daemons consuming 39.6 % of
one core**, several days old:

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
