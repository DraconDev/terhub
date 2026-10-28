//! What a table row must match to stay visible.
//!
//! The live search (include) and the standing filter (exclude) share
//! one notion of "a row matches this text", so they share one helper.

use crate::agent::AgentRecord;
use crate::tmux::Pane;

/// True when `q` (already lowercased) appears in any of the visible
/// columns of the row. Shared by the multi-token include and
/// standing-filter helpers.
///
/// The state matches under both of its names: the canonical one
/// (`limited`) and the one the table shows (`quota`), so a search
/// finds a quota wall by either word.
fn pane_match_any(p: &Pane, rec: Option<&AgentRecord>, q: &str) -> bool {
    let hit = |s: &str| s.to_lowercase().contains(q);
    hit(&p.id)
        || hit(&p.session)
        || hit(&p.window_name)
        || p.index.to_string().contains(q)
        || hit(&p.command)
        || hit(&p.path)
        || rec.is_some_and(|r| {
            hit(&r.agent) || hit(r.state.label()) || hit(r.display_label()) || hit(&r.summary)
        })
}

/// Multi-token OR-combined include: a row is kept when it matches
/// ANY positive (lowercased) token and NONE of the `!`-prefixed
/// ones. Empty tokens are skipped, so a lone `!` constrains
/// nothing and a negation-only query keeps every un-excluded row.
pub(crate) fn pane_matches_include(p: &Pane, rec: Option<&AgentRecord>, tokens: &[&str]) -> bool {
    // Exclusions first: a `!`-hit hides the row no matter which
    // positive token matched, so positives and negations must both
    // be fully evaluated (no early `true` on the first positive).
    let mut any_positive = false;
    let mut positive_hit = false;
    for tok in tokens {
        let tok = tok.trim();
        match tok.strip_prefix('!') {
            Some(term) => {
                let term = term.trim();
                if !term.is_empty() && pane_match_any(p, rec, term) {
                    return false;
                }
            }
            None => {
                if tok.is_empty() {
                    continue;
                }
                any_positive = true;
                positive_hit = positive_hit || pane_match_any(p, rec, tok);
            }
        }
    }
    !any_positive || positive_hit
}

/// Multi-token NOT-OR for the standing filter: a row is kept only
/// if it matches NONE of the (lowercased) tokens. A leading `!` on
/// any token is stripped before matching so the prompt's
/// pre-filled `!` and the user-typed `!` are both treated as a
/// bare exclude. Empty tokens are skipped.
pub(crate) fn pane_matches_excludes(p: &Pane, rec: Option<&AgentRecord>, tokens: &[&str]) -> bool {
    for tok in tokens {
        let stripped = tok.trim().trim_start_matches('!');
        if !stripped.is_empty() && pane_match_any(p, rec, stripped) {
            return false;
        }
    }
    true
}

/// Table legend, shown when `?` is pressed twice: the marks and column
/// shorthands the table itself never explains. Ordered so the marks
/// come first — a narrow terminal truncates the tail, never the
/// characters the user asked about.
pub(crate) const LEGEND: &str = "legend: !=needs you · +=marked · *=active pane (multi-pane window) · ✓=seen via jump/attend · ●/○=attached (top bar) · Src hook/proc/scr · For=in this state · Turn=last finished turn · Idle=since screen changed · Activity=output/tick";
