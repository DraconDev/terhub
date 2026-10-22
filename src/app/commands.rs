//! Custom command templates and spawn window names.
//!
//! Both are about *launching* something: a custom command from the
//! config, and the window name a freshly spawned agent gets.

use crate::agent::AgentRecord;

/// Window name for a spawn: `{profile}-{first words of prompt}`,
/// sanitized to tmux-safe chars and capped so tabs stay readable.
/// Empty prompt degrades to the bare profile name.
pub fn spawn_window_name(profile: &str, prompt: &str) -> String {
    let clean = |s: &str| {
        s.chars()
            .filter(|c| c.is_ascii_alphanumeric() || *c == '-' || *c == '_')
            .collect::<String>()
    };
    let prof = clean(profile);
    let prof = if prof.is_empty() {
        "agent".to_string()
    } else {
        prof
    };
    let slug: String = prompt
        .split_whitespace()
        .take(3)
        .collect::<Vec<_>>()
        .join("-")
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || *c == '-' || *c == '_')
        .take(20)
        .collect();
    if slug.is_empty() {
        prof.chars().take(32).collect()
    } else {
        format!("{prof}-{slug}").chars().take(32).collect()
    }
}

/// Expand a custom command template against the selected row. Unknown
/// `{var}`s stay literal (config validation rejects them at load; this is
/// the belt-and-braces path for programmatically built configs).
pub fn expand_command(
    template: &str,
    pane: &crate::tmux::Pane,
    rec: Option<&AgentRecord>,
) -> String {
    let agent = rec.map(|r| r.agent.as_str()).unwrap_or("");
    let state = rec.map(|r| r.state.label()).unwrap_or("");
    let value = |var: &str| -> Option<&str> {
        match var {
            "pane" => Some(pane.id.as_str()),
            "session" => Some(pane.session.as_str()),
            "window" => Some(pane.window_name.as_str()),
            "dir" => Some(pane.path.as_str()),
            "cmd" => Some(pane.command.as_str()),
            "agent" => Some(agent),
            "state" => Some(state),
            _ => None,
        }
    };
    let chars: Vec<char> = template.chars().collect();
    let mut out = String::new();
    let mut i = 0;
    while i < chars.len() {
        if chars[i] == '}' && chars.get(i + 1) == Some(&'}') {
            out.push('}');
            i += 2;
            continue;
        }
        if chars[i] == '{' {
            if chars.get(i + 1) == Some(&'{') {
                out.push('{');
                i += 2;
                continue;
            }
            if let Some(end) = chars[i..].iter().position(|c| *c == '}') {
                let var: String = chars[i + 1..i + end].iter().collect();
                match value(&var) {
                    Some(v) => out.push_str(&crate::fleet::sh_quote(v)),
                    None => {
                        out.push('{');
                        out.push_str(&var);
                        out.push('}');
                    }
                }
                i += end + 1;
                continue;
            }
        }
        out.push(chars[i]);
        i += 1;
    }
    out
}
