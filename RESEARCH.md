# Ecosystem research: what others do that terhub doesn't

Researched 2026-10-01 against terhub's baseline (TUI table, 3-signal daemon,
hooks, attend/peek, history, automation, MCP, web, fleet — see GUIDE.md).
Every item below is something terhub lacks, with provenance. Ranked by
impact/effort for a solo-user Rust TUI+daemon tmux hub.

## Sources

| Tool | What it is | URL |
|---|---|---|
| muxa | Rust tmux agent observability + orchestration (closest rival) | https://github.com/Open330/muxa |
| claude-squad | Go TUI managing agents in git worktrees | https://github.com/smtg-ai/claude-squad |
| zellij-project-sidebar | Zellij WASM sidebar: AI activity + attention | https://github.com/ogglord/zellij-project-sidebar |
| Zellij | Features: session mgr, resurrection, web client, plugins | https://github.com/zellij-org/zellij-org.github.io/blob/main/content/features.md |
| WezTerm mux | Persistent mux servers, domains, Lua automation | https://wezfurlong.org/wezterm/ |
| sesh | Go session switcher: zoxide + fzf + per-project config | https://github.com/joshmedeski/sesh |
| tms | Rust repo fuzzy-finder into sessions, worktree-aware | https://github.com/jrmoulton/tmux-sessionizer |
| tmuxinator/tmuxp/smug | YAML project templates, freeze, lifecycle hooks | https://github.com/tmuxinator/tmuxinator |
| tmux-resurrect/continuum | Save/restore, autosave, restore-on-boot | https://github.com/tmux-plugins/tmux-resurrect |
| k9s | K8s TUI: marks, command bar, rich filters | https://github.com/derailed/k9s |
| lazygit/lazydocker | Custom commands, command log, side pane, sparklines | https://github.com/jesseduffield/lazygit |
| btop | Pause, themes dir, tree view | https://github.com/aristocratos/btop |
| ttyd | Terminal-over-websocket reference | https://github.com/tsl0922/ttyd |

## Quick wins (each ~a day)

1. **Quota is telemetry, not error.** muxa: "Quota percentages and credits
   are telemetry; an actual limit failure is needed before showing a quota
   error." terhub badges quota-waiting panes as `error` (live case: a pane
   reading "ready to generate the moment quota returns"). Fix: quota
   patterns yield a distinct honest state/summary, error reserved for real
   failures.
2. **Timeouts as config.** Stale 30min, stuck 10min, poll 2s are hardcoded.
   zellij-sidebar uses `SIDEBAR_STALE_TIMEOUT` (default 3600s). Put all
   three in `config.toml`.
3. **Hook spool + backfill.** terhub hooks fail loudly (exit 1) when the
   daemon is down, and a restart goes blind (stuck needs 10min of observed
   stillness). Sidebar persists per-pane state files; muxa has `sync` to
   backfill the registry by scanning. Spool hook events to disk on daemon
   failure, replay on start.
4. **`init` that wires agent hooks.** `terhub config --init` exists, but
   hooks stay unused because wiring `~/.claude/settings.json` etc. is
   manual. muxa `init --component agent-instructions,agent-skills,agent-mcp`
   installs hooks/skills/MCP into detected Claude/Codex installs. This is
   the highest-leverage adoption fix: hooks are terhub's best signal and
   almost nobody configures them.
5. **Stats/report upgrades.** muxa: `stats --graph` (WACT over time),
   `--verbose` diagnostics, group-by project, `--exclude-pane/session`
   glob patterns for monitor scopes; `report --markdown/--json` export;
   `timeline --sort waiting`, `--view heatmap`. terhub has none of these.
6. **Turn durations.** Sidebar shows live "working for Xm" plus "last turn
   took Ym" when idle. terhub's ledger already records transitions, so
   last-turn duration is derivable — expose it (Age column or peek).
7. **Read agent session logs.** muxa reads Codex rollout logs every 2s to
   recover turns when hooks miss. terhub's 4th signal could be Claude
   JSONL / Codex logs: exact turn boundaries without hooks.
8. **Serve hardening.** terhub `serve` is loopback-only but has no auth and
   POST send is always on. ttyd's model: readonly by default, `-W` enables
   writing, `-c` basic credential, unix-socket bind, origin check, auth
   header for proxies. Adopt: `--writable` flag, token auth, socket bind.
9. **Per-pane web deep links.** Zellij web: bookmarkable session URLs
   (`…/my-project`), built-in auth, terminal-optional. terhub web should
   get `?pane=%69` links at minimum; websocket live-terminal (xterm.js)
   is the later upgrade over polling captures.
10. **TUI smalls.** Pause/freeze refresh while inspecting (btop);
    inverse (`/!foo`) and field-scoped (`state:waiting`) filters (k9s); a
    command-log drawer showing what the TUI ran (lazygit).

## Medium (each ~a week)

11. **TUI power features.** Space-mark multi-select + bulk kill/attend,
    `:` command bar, user-movable/hideable columns (k9s); persistent side
    detail pane, per-pane CPU sparkline reusing the stuck sampler
    (lazydocker); config-defined custom commands on the selected row
    (lazygit); user theme-file directory (btop); tree/grouped
    session→window→pane toggle (btop).
12. **Project layer.** Fuzzy project switching with zoxide + git-derived
    auto-naming + live preview (sesh); repo finder with worktrees-as-
    windows, bookmarks, last-session bounce (tms); YAML project templates
    with windows/panes/layouts/startup commands + freeze-live-session
    (tmuxinator/tmuxp/smug); lifecycle hooks (smug); attach-or-create
    named after cwd (tat).
13. **Snapshot/restore workspace.** resurrect (sessions, layout, cwd,
    programs, pane contents), continuum (autosave + restore on boot),
    `muxa snapshot/restore/reload`. Note resurrect.wezterm's trigger:
    save the instant the app loses focus, not just on a timer.
14. **Agent launcher.** New-session-with-prompt, named program profiles
    picker (claude-squad); allowlisted `agent start` with auto tmux/PTY
    host (muxa); per-task git worktrees (claude-squad); pause/resume;
    review-diff tab before applying (claude-squad).
15. **Arbitrary process rows.** `muxa register --name X --pid N` surfaces
    any background process as a pid-tracked row; `muxa run --detach`
    runs a command in an owned PTY that appears as a task. terhub's
    table could track builds/watchers, not just agents.
16. **Fleet depth.** Host labels/annotations + selectors, `-o wide`,
    persistent SSH relay with last-known cache (terhub re-queries every
    time), `fleet watch` TUI (muxa).
17. **Peek upgrade.** muxa peek: live screens dimmed under the overlay,
    latest prompt/response per pane, how long since you last prompted it,
    scrollable reader. terhub peek is a digit list; this is its future.
18. **Prompt history.** `muxa recap`: recent prompts from retained disk
    history. terhub's ledger holds summaries; expose per-pane prompt
    history.
19. **Onboard tour.** `muxa onboard`: 16 live steps on a throwaway tmux
    server + daemon that never touches yours (`--print` for a static
    guide). Best adoption tool seen anywhere here.
20. **Human-side stats.** muxa tracks foreground time + human thinking
    time (HUMAN/THINK/ACT). terhub's ledger already logs human focus —
    surface it in stats.

## Big bets (multi-week, decide deliberately)

21. **Agent messaging/mailbox + skills.** muxa: same-window agent rooms,
    `msg/peers/identity`, reusable `/` prompt-skill templates, MCP peer
    calls, durable history in sqlite. Powerful, but a second product.
22. **Work model.** muxa: Workspace→Work→Run→Agent with work-card
    dashboard, prompt/abort-all per Work, pipeline presets
    (solo/pair/triad), issue-tracker references. Only if terhub grows
    from observer to orchestrator.
23. **More backends.** muxa observes tmux + rmux + herdr simultaneously
    (zellij CLI baseline). terhub: tmux full, zellij listing-only.

## Where terhub already leads (don't regress)

Proc-tree identity through wrappers (`muse` vs `claude`, `pi`), CPU-based
stuck detection, prompt/error Summary lines with context walk, harness
transparency, anonymous-state pairing, 6 themes + density, fullscreen
mouse-first preview, attention-first sort/filter. muxa has no per-pane
summary line and no stuck-via-CPU; sidebar has no proc identity.

## Notes

- claude-squad is AGPL-3.0: ideas fine, code not reusable.
- Unresolved from researchers: lazydocker's exact multi-select keys and
  atac's mouse support unconfirmed (minor; don't plan on them).
