//! Agent hook installers: teach claude/pi to report lifecycle to terhub.
//!
//! Installers only ADD our entries (idempotent re-runs change nothing);
//! uninstall only removes entries carrying our marker, never user hooks.
//! A `.terhub-bak` copy guards the first write to a foreign file.
use anyhow::{Context, Result};
use std::path::{Path, PathBuf};

/// Marker identifying our hook entries (uninstall matches on this).
pub const MARKER: &str = "terhub hook";
const BACKUP_SUFFIX: &str = ".terhub-bak";

/// Parse a hook-stdin summary: a JSON `{"message": ...}` wins, else the
/// first raw line. Capped so a chatty hook can't flood the ledger.
pub fn stdin_summary(raw: &str) -> String {
    const CAP: usize = 200;
    let trimmed = raw.trim();
    if let Ok(v) = serde_json::from_str::<serde_json::Value>(trimmed)
        && let Some(m) = v.get("message").and_then(|m| m.as_str())
        && !m.trim().is_empty()
    {
        return crate::agent::cap_chars(m.trim(), CAP);
    }
    crate::agent::cap_chars(trimmed.lines().next().unwrap_or(""), CAP)
}

/// Prompt text from hook stdin: Claude's UserPromptSubmit JSON carries the
/// prompt under "prompt"; anything else degrades to the raw head line.
pub fn stdin_prompt(raw: &str) -> String {
    let trimmed = raw.trim();
    if let Ok(v) = serde_json::from_str::<serde_json::Value>(trimmed)
        && let Some(p) = v.get("prompt").and_then(|p| p.as_str())
        && !p.trim().is_empty()
    {
        return crate::agent::cap_chars(p.trim(), crate::history::LEDGER_PROMPT_CAP);
    }
    crate::agent::cap_chars(
        trimmed.lines().next().unwrap_or(""),
        crate::history::LEDGER_PROMPT_CAP,
    )
}

/// One managed hook event: settings key, reported state, summary source.
struct HookDef {
    event: &'static str,
    state: &'static str,
    /// Static summary, or None to read it from hook stdin.
    summary: Option<&'static str>,
    /// The event carries the user's prompt on stdin (prompt ledger).
    prompt_stdin: bool,
}

const CLAUDE_HOOKS: &[HookDef] = &[
    HookDef {
        event: "SessionStart",
        state: "working",
        summary: Some("session started"),
        prompt_stdin: false,
    },
    HookDef {
        event: "UserPromptSubmit",
        state: "working",
        // Static: this stdin carries "prompt", not "message", so
        // --summary-stdin would keep JSON garbage as the summary.
        summary: Some("prompt submitted"),
        prompt_stdin: true,
    },
    HookDef {
        event: "Notification",
        state: "waiting",
        summary: None,
        prompt_stdin: false,
    },
    HookDef {
        event: "Stop",
        // Stop means the turn *finished*, not that it is blocked. Mapping
        // it to waiting made every completed Claude agent demand
        // attention; idle is the honest state.
        state: "idle",
        summary: Some("response finished"),
        prompt_stdin: false,
    },
];

/// How many Claude events a full install wires (status "installed").
pub fn claude_expected_events() -> usize {
    CLAUDE_HOOKS.len()
}

fn hook_command(agent: &str, def: &HookDef) -> String {
    let summary = match def.summary {
        Some(s) => format!("--summary {}", crate::fleet::sh_quote(s)),
        None => "--summary-stdin".to_string(),
    };
    let prompt = if def.prompt_stdin {
        " --prompt-stdin"
    } else {
        ""
    };
    format!(
        "[ -n \"$TMUX_PANE\" ] && terhub hook --agent {agent} --pane \"$TMUX_PANE\" --state {} {summary}{prompt}",
        def.state
    )
}

/// Claude settings path (override seam for tests and power users).
pub fn claude_settings_path() -> PathBuf {
    if let Ok(p) = std::env::var("TERHUB_CLAUDE_SETTINGS")
        && !p.is_empty()
    {
        return PathBuf::from(p);
    }
    home_dir().join(".claude").join("settings.json")
}

/// pi extension path (override seam for tests and power users).
pub fn pi_extension_path() -> PathBuf {
    if let Ok(p) = std::env::var("TERHUB_PI_EXTENSION")
        && !p.is_empty()
    {
        return PathBuf::from(p);
    }
    home_dir()
        .join(".pi")
        .join("agent")
        .join("extensions")
        .join("terhub.ts")
}

fn home_dir() -> PathBuf {
    std::env::var("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from("/tmp"))
}

/// Back up `path` once (first managed write); missing file needs none.
fn ensure_backup(path: &Path) -> Result<()> {
    if !path.is_file() {
        return Ok(());
    }
    let backup = path.with_extension(format!(
        "{}{BACKUP_SUFFIX}",
        path.extension().and_then(|e| e.to_str()).unwrap_or("json")
    ));
    if backup.exists() {
        return Ok(());
    }
    std::fs::copy(path, &backup).with_context(|| format!("backup {}", path.display()))?;
    Ok(())
}

/// True when `doc` already carries a terhub hook for `event` under any
/// matcher (not just the first). The earlier `pointer("/hooks/.../0/...")`
/// check missed matchers at index 1+, which made `claude_status` lie
/// and `claude_install` non-idempotent (a second run added a duplicate
/// that fired the event twice).
fn has_marker(doc: &serde_json::Value, event: &str) -> bool {
    doc.pointer(&format!("/hooks/{event}", event = event))
        .and_then(|v| v.as_array())
        .is_some_and(|arr| {
            arr.iter().any(|matcher| {
                matcher
                    .get("hooks")
                    .and_then(|h| h.as_array())
                    .is_some_and(|hooks| {
                        hooks.iter().any(|h| {
                            h.get("command")
                                .and_then(|c| c.as_str())
                                .is_some_and(|c| c.contains(MARKER))
                        })
                    })
            })
        })
}

/// Install our hooks into a Claude `settings.json` (created when missing).
/// Returns per-event whether it was added (false = already present).
pub fn claude_install(path: &Path) -> Result<Vec<(&'static str, bool)>> {
    let mut doc: serde_json::Value = match std::fs::read_to_string(path) {
        Ok(text) if text.trim().is_empty() => serde_json::Value::Null,
        Ok(text) => serde_json::from_str(&text)
            .with_context(|| format!("{} is not valid JSON (left untouched)", path.display()))?,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => serde_json::Value::Null,
        Err(e) => return Err(e).with_context(|| format!("read {}", path.display())),
    };
    if doc.is_null() {
        doc = serde_json::json!({});
    }
    if !doc.is_object() {
        anyhow::bail!("{} is not a JSON object (left untouched)", path.display());
    }
    let mut report = Vec::new();
    let mut changed = false;
    for def in CLAUDE_HOOKS {
        let present = has_marker(&doc, def.event);
        report.push((def.event, !present));
        if present {
            continue;
        }
        changed = true;
        let root = doc.as_object_mut().unwrap();
        if !root.get("hooks").is_some_and(|v| v.is_object()) {
            root.insert("hooks".to_string(), serde_json::json!({}));
        }
        let hooks = root.get_mut("hooks").unwrap().as_object_mut().unwrap();
        let entry = hooks
            .entry(def.event.to_string())
            .or_insert_with(|| serde_json::json!([]));
        if !entry.is_array() {
            *entry = serde_json::json!([]);
        }
        let arr = entry.as_array_mut().unwrap();
        // Merge into the first matcher when it already holds hooks (keeps
        // one matcher per event); else append a fresh matcher.
        let slot = arr.iter_mut().find(|m| {
            m.get("hooks")
                .and_then(|h| h.as_array())
                .is_some_and(|h| !h.is_empty())
        });
        let hook = serde_json::json!({"type": "command", "command": hook_command("claude", def)});
        match slot {
            Some(m) => m["hooks"].as_array_mut().unwrap().push(hook),
            None => arr.push(serde_json::json!({"hooks": [hook]})),
        }
    }
    if changed {
        ensure_backup(path)?;
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let mut text = serde_json::to_string_pretty(&doc)?;
        text.push('\n');
        std::fs::write(path, text)?;
    }
    Ok(report)
}

/// Remove our hooks from a Claude `settings.json`. Returns events cleaned.
pub fn claude_uninstall(path: &Path) -> Result<Vec<&'static str>> {
    let text = match std::fs::read_to_string(path) {
        Ok(t) => t,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(e).with_context(|| format!("read {}", path.display())),
    };
    let mut doc: serde_json::Value = serde_json::from_str(&text)
        .with_context(|| format!("{} is not valid JSON (left untouched)", path.display()))?;
    let mut cleaned = Vec::new();
    for def in CLAUDE_HOOKS {
        let mut removed = false;
        if let Some(arr) = doc
            .pointer_mut(&format!("/hooks/{}", def.event))
            .and_then(|v| v.as_array_mut())
        {
            for matcher in arr.iter_mut() {
                if let Some(hooks) = matcher.get_mut("hooks").and_then(|h| h.as_array_mut()) {
                    let before = hooks.len();
                    hooks.retain(|h| {
                        !h.get("command")
                            .and_then(|c| c.as_str())
                            .is_some_and(|c| c.contains(MARKER))
                    });
                    removed |= hooks.len() != before;
                }
            }
            // Drop matchers we emptied (user matchers with other hooks stay).
            arr.retain(|m| {
                m.get("hooks")
                    .and_then(|h| h.as_array())
                    .is_none_or(|h| !h.is_empty())
            });
        }
        if removed {
            cleaned.push(def.event);
        }
    }
    if !cleaned.is_empty() {
        ensure_backup(path)?;
        let mut text = serde_json::to_string_pretty(&doc)?;
        text.push('\n');
        std::fs::write(path, text)?;
    }
    Ok(cleaned)
}

/// Events currently carrying our hooks (empty file counts as none).
pub fn claude_status(path: &Path) -> Vec<&'static str> {
    let Ok(text) = std::fs::read_to_string(path) else {
        return Vec::new();
    };
    let Ok(doc) = serde_json::from_str::<serde_json::Value>(&text) else {
        return Vec::new();
    };
    CLAUDE_HOOKS
        .iter()
        .filter(|def| has_marker(&doc, def.event))
        .map(|def| def.event)
        .collect()
}

/// pi lifecycle reporter (no type imports: pi's package namespace has
/// forked, and this file must load under any of them).
pub const PI_EXTENSION: &str = r#"// terhub lifecycle reporter (managed by `terhub hooks`; do not edit).
// Forwards pi events to the terhub daemon; never throws into pi.
import { execFileSync } from "node:child_process";

function report(state: string, summary: string, prompt?: string): void {
  try {
    const pane = process.env.TMUX_PANE;
    if (!pane) return;
    const args = ["hook", "--agent", "pi", "--pane", pane, "--state", state, "--summary", summary];
    if (prompt) args.push("--prompt", prompt.slice(0, 500));
    execFileSync("terhub", args, { stdio: "ignore", timeout: 2000 });
  } catch {
    // Hooks must never break the agent they observe.
  }
}

// Model-callable tools that block until the human answers. Without
// these, a question looks identical to work: pi fires no lifecycle
// event while the menu is open, so the last `turn_start` working
// report stands until `agent_end` — which never comes while the
// agent is blocked.
const QUESTION_TOOLS = ["question", "questionnaire"];

// Best human-readable question text from a question tool's input:
// `questionnaire` carries `questions[].prompt`, the single `question`
// tool carries `question`.
function questionText(event: any): string {
  const input = event && event.input;
  if (!input || typeof input !== "object") return "";
  const qs = input.questions;
  if (Array.isArray(qs) && qs.length > 0) {
    const p = qs[0] && typeof qs[0].prompt === "string" ? qs[0].prompt : "";
    return p;
  }
  return typeof input.question === "string" ? input.question : "";
}

export default function (pi: any): void {
  // True while a blocking prompt is open. Both routes set it so the
  // generic `ui_prompt_*` events never clobber the richer question
  // text a tool call already reported.
  let awaiting = false;

  pi.on("session_start", async () => {
    awaiting = false;
    report("working", "session started");
  });
  pi.on("before_agent_start", async (event: any) => {
    awaiting = false;
    const prompt = event && typeof event.prompt === "string" ? event.prompt : "";
    report("working", "turn started", prompt);
  });
  pi.on("turn_start", async () => {
    awaiting = false;
    report("working", "turn started");
  });
  pi.on("agent_end", async () => {
    awaiting = false;
    report("idle", "response finished");
  });
  pi.on("session_shutdown", async () => {
    awaiting = false;
    report("idle", "session ended");
  });

  // Question tools: report waiting with the actual question, then
  // back to working once the answer (or a cancel) arrives. Returning
  // undefined leaves execution — and the prompt — untouched.
  const isQuestionTool = (event: any): boolean =>
    !!(event && typeof event.toolName === "string" && QUESTION_TOOLS.indexOf(event.toolName) !== -1);

  pi.on("tool_call", async (event: any) => {
    if (!isQuestionTool(event)) return undefined;
    awaiting = true;
    report("waiting", questionText(event) || "question");
    return undefined;
  });
  pi.on("tool_result", async (event: any) => {
    if (!isQuestionTool(event) || !awaiting) return undefined;
    awaiting = false;
    report("working", "answer received");
    return undefined;
  });

  // Any other blocking user-facing prompt (permission confirms,
  // extension dialogs) is a needs-you reason too, when Pi reports one.
  pi.on("ui_prompt_start", async (event: any) => {
    if (awaiting) return undefined;
    awaiting = true;
    const title = event && typeof event.title === "string" && event.title ? event.title : "";
    const kind = event && typeof event.kind === "string" && event.kind ? event.kind : "input";
    report("waiting", title || `waiting on ${kind}`);
    return undefined;
  });
  pi.on("ui_prompt_end", async () => {
    if (!awaiting) return undefined;
    awaiting = false;
    report("working", "answer received");
    return undefined;
  });
}
"#;

/// Install the pi reporter. Ok(true) = written or updated, Ok(false) =
/// already current. Content-idempotent: a stale ours-file (marker, old
/// body) is rewritten so hook upgrades actually land.
pub fn pi_install(path: &Path) -> Result<bool> {
    if path.is_file() {
        let text = std::fs::read_to_string(path)?;
        if text == PI_EXTENSION {
            return Ok(false);
        }
        // Ours but stale, or foreign: back up once, then overwrite.
        ensure_backup(path)?;
    }
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(path, PI_EXTENSION)?;
    Ok(true)
}

/// Remove the pi reporter, but only when it carries our marker.
/// Ok(true) = removed, Ok(false) = missing or foreign (kept).
pub fn pi_uninstall(path: &Path) -> Result<bool> {
    if !path.is_file() {
        return Ok(false);
    }
    let text = std::fs::read_to_string(path)?;
    if !text.contains(MARKER) {
        return Ok(false);
    }
    ensure_backup(path)?;
    std::fs::remove_file(path)?;
    Ok(true)
}

pub fn pi_status(path: &Path) -> bool {
    path.is_file() && std::fs::read_to_string(path).is_ok_and(|t| t.contains(MARKER))
}

/// An installed reporter that isn't the shipped template: it loads
/// fine but misses newer events, so `hooks status` can tell the user
/// to reinstall instead of claiming everything is current.
pub fn pi_stale(path: &Path) -> bool {
    pi_status(path) && std::fs::read_to_string(path).is_ok_and(|t| t != PI_EXTENSION)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("terhub-hooks-{}-{tag}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn stdin_summary_prefers_message_then_raw_head() {
        assert_eq!(stdin_summary(r#"{"message": " Approve? "}"#), "Approve?");
        assert_eq!(stdin_summary("raw line\nsecond"), "raw line");
        assert_eq!(stdin_summary("   "), "");
        assert_eq!(stdin_summary(r#"{"message": 42}"#), r#"{"message": 42}"#);
        let long = "x".repeat(500);
        assert_eq!(stdin_summary(&long).len(), 200);
        let msg = format!(r#"{{"message": "{}"}}"#, "y".repeat(500));
        assert_eq!(stdin_summary(&msg).len(), 200);
    }

    #[test]
    fn stdin_prompt_prefers_prompt_field_then_head() {
        assert_eq!(stdin_prompt(r#"{"prompt": " fix it "}"#), "fix it");
        assert_eq!(stdin_prompt("raw line\nsecond"), "raw line");
        assert_eq!(stdin_prompt("   "), "");
        // Not the summary's message field: prompts come from "prompt".
        assert_eq!(stdin_prompt(r#"{"message": "hi"}"#), r#"{"message": "hi"}"#);
        let long = "x".repeat(900);
        assert_eq!(stdin_prompt(&long).chars().count(), 500);
    }

    #[test]
    fn turn_end_reports_idle_not_waiting() {
        // "response finished" means the turn is over. Reporting it as
        // waiting made every completed agent demand attention.
        let dir = scratch("turnend");
        let claude = dir.join("settings.json");
        claude_install(&claude).unwrap();
        let text = std::fs::read_to_string(&claude).unwrap();
        assert!(
            text.contains("--state idle --summary 'response finished'"),
            "claude Stop must report idle:\n{text}"
        );
        // Only a real notification asks for your attention.
        assert!(text.contains("--state waiting"), "{text}");

        let pi = dir.join("terhub.ts");
        pi_install(&pi).unwrap();
        let ts = std::fs::read_to_string(&pi).unwrap();
        assert!(
            ts.contains(r#"report("idle", "response finished")"#),
            "pi agent_end must report idle:\n{ts}"
        );
        assert!(
            !ts.contains(r#"report("waiting", "response finished")"#),
            "{ts}"
        );
    }

    #[test]
    fn claude_install_wires_user_prompt_submit() {
        let dir = scratch("promptsubmit");
        let path = dir.join("settings.json");
        claude_install(&path).unwrap();
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.contains("UserPromptSubmit"), "{text}");
        assert!(text.contains("--prompt-stdin"), "{text}");
        // And an existing three-event install gains only the new event.
        let dir2 = scratch("promptsubmit2");
        let path2 = dir2.join("settings.json");
        std::fs::write(
            &path2,
            r#"{"hooks": {"SessionStart": [{"hooks": [{"type": "command", "command": "[ -n \"$TMUX_PANE\" ] && terhub hook --agent claude"}]}]}}"#,
        )
        .unwrap();
        let report = claude_install(&path2).unwrap();
        assert_eq!(report.len(), 4);
        assert!(
            report
                .iter()
                .find(|(e, _)| *e == "SessionStart")
                .is_some_and(|(_, added)| !added)
        );
        assert!(
            report
                .iter()
                .find(|(e, _)| *e == "UserPromptSubmit")
                .is_some_and(|(_, added)| *added)
        );
    }

    #[test]
    fn pi_install_updates_stale_reporter() {
        let dir = scratch("piupdate");
        let path = dir.join("terhub.ts");
        // A v1 install (marker, old body) is rewritten in place.
        std::fs::write(&path, "// ours\nterhub hook --agent pi\n").unwrap();
        assert!(pi_install(&path).unwrap());
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.contains("before_agent_start"), "{text}");
        assert!(text.contains("--prompt"), "{text}");
        assert!(!pi_install(&path).unwrap());
        // The shipped template carries the prompt handler.
        assert!(PI_EXTENSION.contains("before_agent_start"));
        // A fresh install is current, a rewritten stale file is not
        // (status uses this to tell the user to reinstall).
        assert!(!pi_stale(&path));
    }

    #[test]
    fn pi_stale_detects_older_reporter() {
        let dir = scratch("pistale");
        let path = dir.join("terhub.ts");
        // Nothing on disk: not installed, so not stale.
        assert!(!pi_stale(&path));
        // An older body: our marker, missing the question handlers.
        std::fs::write(&path, "// ours\nterhub hook --agent pi\n").unwrap();
        assert!(pi_stale(&path), "older reporter must read as stale");
        // Reinstalling makes it current again.
        pi_install(&path).unwrap();
        assert!(!pi_stale(&path), "reinstall must clear staleness");
    }

    #[test]
    fn pi_reporter_reports_questions_as_waiting() {
        // A question tool blocks the agent and fires no lifecycle
        // event, so without these handlers the last `turn_start`
        // working report stands until `agent_end` — which never
        // comes while the menu is open. This is the false-negative
        // family: panes sitting at a question menu read `working`.
        assert!(
            PI_EXTENSION.contains("tool_call"),
            "reporter must observe tool calls\n{PI_EXTENSION}"
        );
        assert!(
            PI_EXTENSION.contains("questionnaire") && PI_EXTENSION.contains("\"question\""),
            "reporter must recognize both question tools\n{PI_EXTENSION}"
        );
        // The waiting report carries the question text as the summary
        // (questions[].prompt / question) so the row chip shows what
        // the agent asked, not a generic marker.
        assert!(
            PI_EXTENSION.contains("qs[0].prompt"),
            "reporter must surface the question text\n{PI_EXTENSION}"
        );
        assert!(
            PI_EXTENSION.contains("report(\"waiting\""),
            "question must report waiting\n{PI_EXTENSION}"
        );
        // The answer clears it: a pane the human just answered is
        // working again, and staying waiting would be a false alarm.
        assert!(
            PI_EXTENSION.contains("answer received"),
            "answer must restore working\n{PI_EXTENSION}"
        );
        // Generic blocking prompts (permission confirms, extension
        // dialogs) count too — same handling, coarser summary.
        assert!(
            PI_EXTENSION.contains("ui_prompt_start"),
            "reporter must observe blocking UI prompts\n{PI_EXTENSION}"
        );
        // ...and the awaiting guard keeps the two routes from
        // clobbering each other's summary.
        assert!(
            PI_EXTENSION.contains("let awaiting = false"),
            "reporter needs the awaiting guard\n{PI_EXTENSION}"
        );
    }

    #[test]
    fn claude_install_merges_idempotently() {
        let dir = scratch("install");
        let path = dir.join("settings.json");
        // Missing file: created with our four events.
        let report = claude_install(&path).unwrap();
        assert_eq!(report.len(), 4);
        assert!(report.iter().all(|(_, added)| *added));
        let text = std::fs::read_to_string(&path).unwrap();
        assert_eq!(text.matches(MARKER).count(), 4);
        assert_eq!(claude_status(&path).len(), 4);
        // Idempotent: second run adds nothing, writes nothing new.
        let before = text.clone();
        let report = claude_install(&path).unwrap();
        assert!(report.iter().all(|(_, added)| !added));
        assert_eq!(std::fs::read_to_string(&path).unwrap(), before);
        // Existing user hooks survive alongside ours.
        let dir2 = scratch("merge");
        let path2 = dir2.join("settings.json");
        std::fs::write(
            &path2,
            r#"{"hooks": {"SessionStart": [{"hooks": [{"type": "command", "command": "echo hi"}]}]}, "other": 1}"#,
        )
        .unwrap();
        claude_install(&path2).unwrap();
        let text2 = std::fs::read_to_string(&path2).unwrap();
        assert!(text2.contains("echo hi"), "{text2}");
        assert!(text2.contains("\"other\": 1"), "{text2}");
        assert_eq!(text2.matches(MARKER).count(), 4);
        assert!(path2.with_extension("json.terhub-bak").exists());
    }

    #[test]
    fn claude_install_and_status_see_hooks_in_any_matcher() {
        // A user with a first matcher that holds unrelated hooks and a
        // second matcher that holds ours must still count as
        // "already installed": the old `pointer("/hooks/<event>/0/...")`
        // check only looked at matcher 0, so the second run added a
        // duplicate and the event fired twice.
        let dir = scratch("matcher-any");
        let path = dir.join("settings.json");
        // Build a settings file where matcher 0 has a non-terhub hook
        // and matcher 1 has ours, for all four events.
        let mut doc = serde_json::json!({"hooks": {}});
        for def in CLAUDE_HOOKS {
            doc["hooks"][def.event] = serde_json::json!([
                {"hooks": [{"type": "command", "command": "echo user"}]},
                {"hooks": [{"type": "command", "command": hook_command("claude", def)}]},
            ]);
        }
        std::fs::write(&path, serde_json::to_string_pretty(&doc).unwrap()).unwrap();
        // status sees all four events (not 0).
        let status = claude_status(&path);
        assert_eq!(status.len(), CLAUDE_HOOKS.len(), "status: {status:?}");
        // A second install is a no-op: nothing added, nothing changed.
        let before = std::fs::read_to_string(&path).unwrap();
        let report = claude_install(&path).unwrap();
        assert!(report.iter().all(|(_, added)| !*added), "{report:?}");
        assert_eq!(std::fs::read_to_string(&path).unwrap(), before);
        // Each event still has exactly one terhub hook (no duplicate).
        let after = std::fs::read_to_string(&path).unwrap();
        assert_eq!(
            after.matches(MARKER).count(),
            CLAUDE_HOOKS.len(),
            "second install duplicated: {after}"
        );
    }

    #[test]
    fn claude_uninstall_removes_only_ours() {
        let dir = scratch("uninstall");
        let path = dir.join("settings.json");
        std::fs::write(
            &path,
            r#"{"hooks": {"SessionStart": [{"hooks": [{"type": "command", "command": "echo hi"}]}]}}"#,
        )
        .unwrap();
        claude_install(&path).unwrap();
        let cleaned = claude_uninstall(&path).unwrap();
        assert_eq!(cleaned.len(), 4);
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.contains("echo hi"), "{text}");
        assert!(!text.contains(MARKER), "{text}");
        // Missing file is a clean no-op.
        assert!(claude_uninstall(&dir.join("nope.json")).unwrap().is_empty());
    }

    #[test]
    fn claude_refuses_corrupt_file() {
        let dir = scratch("corrupt");
        let path = dir.join("settings.json");
        std::fs::write(&path, "{oops").unwrap();
        assert!(claude_install(&path).is_err());
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "{oops");
        assert!(claude_uninstall(&path).is_err());
    }

    #[test]
    fn pi_install_guards_foreign_files() {
        let dir = scratch("pi");
        let path = dir.join("terhub.ts");
        assert!(pi_install(&path).unwrap());
        assert!(pi_status(&path));
        assert!(!pi_install(&path).unwrap());
        assert!(pi_uninstall(&path).unwrap());
        assert!(!pi_status(&path));
        assert!(!pi_uninstall(&path).unwrap());
        // A foreign file is never touched.
        std::fs::write(&path, "export default {}").unwrap();
        assert!(!pi_uninstall(&path).unwrap());
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "export default {}");
        assert!(pi_install(&path).unwrap());
        assert!(path.with_extension("ts.terhub-bak").exists());
    }
}
