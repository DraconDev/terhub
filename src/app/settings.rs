//! The settings page.
//!
//! The page's rows, the named (saved) filters it manages, and the
//! surgical writes it makes back to the config file. Everything here
//! is about the settings *surface*; the key handlers that drive it
//! stay with the other key handling in the parent module.

use super::*;

impl App {
    /// Rotate the theme live (session-only; the config file is untouched).
    pub fn cycle_theme(&mut self) {
        use crate::config::THEMES;
        // Case-insensitive like is_known_theme/palette(): a config
        // with `theme = "Dark"` loads fine, and cycling must advance
        // from it instead of "cycling" back to dark itself.
        let cur = THEMES
            .iter()
            .position(|t| t.eq_ignore_ascii_case(&self.config.theme));
        let next = cur.map(|i| (i + 1) % THEMES.len()).unwrap_or(0);
        self.config.theme = THEMES[next].to_string();
        self.set_status(format!(
            "theme: {} ({}/{})",
            THEMES[next],
            next + 1,
            THEMES.len()
        ));
    }

    /// Flip table density live (session-only).
    pub fn cycle_density(&mut self) {
        self.config.density = if self.config.compact() {
            "comfortable".to_string()
        } else {
            "compact".to_string()
        };
        self.set_status(format!("density: {}", self.config.density));
    }

    /// Settings popup rows: section headers plus actionable items.
    pub fn settings_rows(&self) -> Vec<SettingRow> {
        use crate::config::{DENSITIES, THEMES};
        let mut rows = vec![SettingRow::Header("Theme")];
        for (i, t) in THEMES.iter().enumerate() {
            rows.push(SettingRow::Item {
                label: t.to_string(),
                active: self.config.palette() == *t,
                kind: SettingKind::Theme(i),
            });
        }
        rows.push(SettingRow::Header("Density"));
        for (i, d) in DENSITIES.iter().enumerate() {
            rows.push(SettingRow::Item {
                label: d.to_string(),
                active: self.config.density_name() == *d,
                kind: SettingKind::Density(i),
            });
        }
        rows.push(SettingRow::Header("Toggles"));
        rows.push(SettingRow::Item {
            label: format!(
                "notifications: {}",
                Self::onoff(self.config.notifications_enabled)
            ),
            active: self.config.notifications_enabled,
            kind: SettingKind::ToggleNotifications,
        });
        rows.push(SettingRow::Item {
            label: format!("mru tabs: {}", Self::onoff(self.config.mru_tabs)),
            active: self.config.mru_tabs,
            kind: SettingKind::ToggleMruTabs,
        });
        rows.push(SettingRow::Item {
            label: format!(
                "highlight attached: {}",
                Self::onoff(self.config.highlight_attached)
            ),
            active: self.config.highlight_attached,
            kind: SettingKind::ToggleHighlight,
        });
        rows.push(SettingRow::Item {
            label: format!(
                "blur columns (record/share): {}",
                Self::onoff(self.config.blur_cols)
            ),
            active: self.config.blur_cols,
            kind: SettingKind::ToggleBlurCols,
        });
        rows.push(SettingRow::Header("Filters"));
        rows.push(SettingRow::Item {
            label: if self.exclude.is_empty() {
                "standing filter (excludes): (none) — Enter to edit".to_string()
            } else {
                format!(
                    "standing filter (excludes): {} — Enter to edit",
                    self.exclude
                )
            },
            active: !self.exclude.is_empty(),
            kind: SettingKind::EditStandingFilter,
        });
        // Named saved filters. One row each: Enter applies the query,
        // `r` renames it, `d` deletes it. The marker names the one
        // currently driving the table.
        for (i, f) in self.saved_filters.iter().enumerate() {
            rows.push(SettingRow::Item {
                label: format!("saved: {} — {}", f.name, Self::truncate_query(&f.query, 24)),
                active: self.active_saved_filter.as_deref() == Some(f.name.as_str()),
                kind: SettingKind::UseSavedFilter(i),
            });
        }
        // Save-the-search row: only worth showing while there is a
        // search to name (the standing exclusion is not a query).
        if !self.filter.is_empty() {
            rows.push(SettingRow::Item {
                label: "save current search as named filter … (n)".to_string(),
                active: false,
                kind: SettingKind::AddSavedFilter,
            });
        }
        // What the two queries actually hide, so the exclusion is
        // inspectable instead of a leap of faith. Partitioned with no
        // double-counting: rows failing the standing test list under
        // it; rows passing that but failing the search list under the
        // search. Read-only rows (never selectable, never actionable).
        // Same tokenization as refresh_filtered: the standing filter
        // is multi-token NOT-OR, so passing the whole string as one
        // token here made this page disagree with the table about
        // which rows the standing filter hides.
        let ex_tokens: Vec<String> = self
            .exclude
            .split_whitespace()
            .map(|t| t.to_lowercase())
            .collect();
        let ex_refs: Vec<&str> = ex_tokens.iter().map(String::as_str).collect();
        let find_tokens: Vec<String> = self
            .filter
            .split_whitespace()
            .map(|t| t.to_lowercase())
            .collect();
        let q_find: Vec<&str> = find_tokens.iter().map(String::as_str).collect();
        let mut hidden_by_exclude: Vec<&Pane> = Vec::new();
        let mut hidden_by_search: Vec<&Pane> = Vec::new();
        for p in &self.panes {
            let rec = self.agents.get(&p.id);
            // The settings page partitions by which single clause
            // hid the row, not by the combined predicate; the
            // combined predicate would double-count when both
            // fire. Both clauses are multi-token (NOT-OR exclude,
            // OR-combined search with per-token `!`).
            if !self.exclude.is_empty() && !pane_matches_excludes(p, rec, &ex_refs) {
                hidden_by_exclude.push(p);
            } else if !self.filter.is_empty() && !pane_matches_include(p, rec, &q_find) {
                hidden_by_search.push(p);
            }
        }
        const HIDDEN_GREY: &str = "#8b949e";
        let hidden_row = |p: &Pane| SettingRow::Legend {
            triangle: "–".to_string(),
            color: HIDDEN_GREY.to_string(),
            name: p.id.clone(),
            meaning: format!("{} · {} · {}", p.session, p.window_name, p.command),
        };
        if self.exclude.is_empty() && self.filter.is_empty() {
            rows.push(SettingRow::Legend {
                triangle: "–".to_string(),
                color: HIDDEN_GREY.to_string(),
                name: "nothing hidden".to_string(),
                meaning: "every pane is shown".to_string(),
            });
        } else {
            if !self.exclude.is_empty() {
                rows.push(SettingRow::Legend {
                    triangle: "–".to_string(),
                    color: HIDDEN_GREY.to_string(),
                    name: format!("{} hidden", hidden_by_exclude.len()),
                    meaning: format!("matching standing filter '{}'", self.exclude),
                });
                for p in &hidden_by_exclude {
                    rows.push(hidden_row(p));
                }
            }
            if !self.filter.is_empty() {
                rows.push(SettingRow::Legend {
                    triangle: "–".to_string(),
                    color: HIDDEN_GREY.to_string(),
                    name: format!("{} hidden", hidden_by_search.len()),
                    meaning: format!("matching search '{}'", self.filter),
                });
                for p in &hidden_by_search {
                    rows.push(hidden_row(p));
                }
            }
        }
        rows.push(SettingRow::Header("Tray icons"));
        for r in crate::tray::icon_legend(self.config.palette()) {
            rows.push(SettingRow::Legend {
                triangle: "▼".to_string(),
                color: r.hex.to_string(),
                name: r.name.to_string(),
                meaning: r.meaning.to_string(),
            });
        }
        rows
    }

    /// Run a saved-filter row action (`r` rename / `d` delete) against
    /// the selected row when that row is a saved filter. Passing the
    /// template kind with index 0 and substituting the real index keeps
    /// the two call sites symmetric.
    pub(crate) fn settings_saved_filter_row(&mut self, template: SettingKind) {
        let items = self.settings_item_rows();
        let Some(row) = items.get(self.settings_selected).copied() else {
            return;
        };
        let kind = match (
            &template,
            self.settings_rows().get(row).and_then(|r| match r {
                SettingRow::Item { kind, .. } => Some(*kind),
                _ => None,
            }),
        ) {
            (SettingKind::RenameSavedFilter(_), Some(SettingKind::UseSavedFilter(i))) => {
                SettingKind::RenameSavedFilter(i)
            }
            (SettingKind::DeleteSavedFilter(_), Some(SettingKind::UseSavedFilter(i))) => {
                SettingKind::DeleteSavedFilter(i)
            }
            _ => return,
        };
        self.apply_setting(kind);
    }

    pub(crate) fn onoff(b: bool) -> &'static str {
        if b { "on" } else { "off" }
    }

    /// Shorten a saved query for a one-line settings row. Cuts at a
    /// char boundary and marks the truncation, so a long multi-token
    /// query never wraps the popup.
    pub(crate) fn truncate_query(q: &str, max: usize) -> String {
        let mut out: String = q.chars().take(max).collect();
        if q.chars().count() > max {
            out.push('…');
        }
        out
    }

    // ---- saved (named) filters -------------------------------------

    /// Persist the saved-filter list to `[ui] saved_filters`. Called
    /// after add / rename / delete so the list survives a restart.
    pub(crate) fn persist_saved_filters(&mut self) {
        self.config.saved_filters.clone_from(&self.saved_filters);
        self.view_changes.saved_filters = true;
        let written = {
            let patch = self.config.ui_persist_changes(&self.view_changes);
            crate::config::persist_ui_settings(&patch)
        };
        match written {
            Ok(()) => self.view_changes.saved_filters = false,
            Err(e) => self.set_status(format!("saved filters not written: {e}")),
        }
    }

    /// Apply saved filter `i`: the live search takes its query and the
    /// table header names it. The standing exclusion is untouched —
    /// a named filter and the standing filter compose.
    pub(crate) fn use_saved_filter(&mut self, i: usize) {
        let Some(f) = self.saved_filters.get(i).cloned() else {
            return;
        };
        self.filter = f.query.clone();
        self.active_saved_filter = Some(f.name.clone());
        self.refresh_filtered();
        self.pane_selected = 0;
        self.pane_scroll = 0;
        self.set_status(format!("filter '{}': {}", f.name, f.query));
    }

    /// Insert or replace a named filter, then persist. A name that
    /// already exists updates that entry instead of adding a duplicate
    /// row (rename and add-by-name converge here).
    pub(crate) fn save_named_filter(&mut self, name: String, query: String) {
        let name = name.trim().to_string();
        let query = query.trim().to_string();
        if name.is_empty() || query.is_empty() {
            return;
        }
        if let Some(existing) = self.saved_filters.iter_mut().find(|f| f.name == name) {
            existing.query = query;
        } else {
            self.saved_filters.push(crate::config::SavedFilter {
                name: name.clone(),
                query,
            });
        }
        self.persist_saved_filters();
    }

    /// Delete saved filter `i`, forgetting its header marker when it
    /// was the active one so the title doesn't name a dead filter.
    pub(crate) fn delete_saved_filter(&mut self, i: usize) {
        let Some(f) = self.saved_filters.get(i) else {
            return;
        };
        let name = f.name.clone();
        self.saved_filters.remove(i);
        if self.active_saved_filter.as_deref() == Some(name.as_str()) {
            self.active_saved_filter = None;
        }
        self.persist_saved_filters();
    }

    /// Name of the saved filter whose query equals the live search, if
    /// any. Lets the title keep naming a saved filter after a restart
    /// when `[ui] filter` matches it exactly.
    pub(crate) fn find_saved_filter_by_query(&self, query: &str) -> Option<String> {
        self.saved_filters
            .iter()
            .find(|f| f.matches_query(query))
            .map(|f| f.name.clone())
    }

    /// Cycle to the next saved filter, wrapping past the last one back
    /// to the first. With no saved filters at all, nothing happens
    /// (the status says so instead of silently doing nothing).
    pub(crate) fn next_saved_filter(&mut self) {
        if self.saved_filters.is_empty() {
            self.set_status("no saved filters (settings › filters › n)".to_string());
            return;
        }
        let next = match self.active_saved_filter.as_deref() {
            None => 0,
            Some(active) => self
                .saved_filters
                .iter()
                .position(|f| f.name == active)
                .map(|i| (i + 1) % self.saved_filters.len())
                .unwrap_or(0),
        };
        self.use_saved_filter(next);
    }

    /// Cycle to the next named saved filter. Shared by bare presses
    /// (either focus) and the modifier-qualified dispatch path so both
    /// spellings behave identically.
    pub(crate) fn cycle_saved_filter(&mut self, tmux: &Tmux) {
        self.next_saved_filter();
        self.follow_preview(tmux);
    }

    /// Clear the search query. Shared like [`Self::cycle_saved_filter`].
    pub(crate) fn clear_filter(&mut self) {
        if self.filter.is_empty() {
            self.set_status("search already clear".to_string());
        } else {
            self.filter.clear();
            self.active_saved_filter = None;
            self.refresh_filtered();
            self.set_status("search cleared".to_string());
        }
    }

    /// Open the naming prompt: `None` names the current search as a new
    /// filter, `Some(i)` renames filter `i`.
    pub(crate) fn open_saved_filter_prompt(&mut self, index: Option<usize>) {
        self.mode = Mode::PromptSavedFilter { index };
        self.input = match index {
            Some(i) => self
                .saved_filters
                .get(i)
                .map(|f| f.name.clone())
                .unwrap_or_default(),
            None => String::new(),
        };
        self.cursor = self.input.chars().count();
    }

    /// Rename saved filter `i` in place, keeping its position and
    /// query. Renaming onto a name another row already holds merges the
    /// two instead of producing two rows with the same label.
    pub(crate) fn rename_saved_filter(&mut self, i: usize, new_name: String) {
        let new_name = new_name.trim().to_string();
        if new_name.is_empty() {
            return;
        }
        let Some(old_name) = self.saved_filters.get(i).map(|f| f.name.clone()) else {
            return;
        };
        if new_name == old_name {
            return;
        }
        if let Some(dup) = self.saved_filters.iter().position(|f| f.name == new_name) {
            self.saved_filters.remove(dup);
            // The removed row may shift the index of the renamed one.
            let target = if dup < i { i - 1 } else { i };
            if let Some(f) = self.saved_filters.get_mut(target) {
                f.name = new_name;
            }
        } else if let Some(f) = self.saved_filters.get_mut(i) {
            f.name = new_name;
        }
        self.persist_saved_filters();
    }

    /// Name typed in the saved-filter prompt: commit it and persist.
    /// `Some(i)` renames (query carried over); `None` names the live
    /// search query.
    pub(crate) fn submit_saved_filter_name(&mut self, index: Option<usize>, name: String) {
        let name = name.trim().to_string();
        if name.is_empty() {
            self.set_status("filter needs a name".to_string());
            return;
        }
        let Some(index) = index else {
            // Adding: the live search is the query.
            if self.filter.trim().is_empty() {
                self.set_status("nothing to save (search is empty)".to_string());
                return;
            }
            let query = self.filter.trim().to_string();
            self.active_saved_filter = Some(name.clone());
            self.save_named_filter(name.clone(), query.clone());
            self.set_status(format!("saved filter '{name}': {query}"));
            return;
        };
        // Renaming: carry the query over and move the header marker when
        // the renamed row was the active one.
        let Some(old) = self
            .saved_filters
            .get(index)
            .map(|f| (f.name.clone(), f.query.clone()))
        else {
            return;
        };
        self.rename_saved_filter(index, name.clone());
        if self.active_saved_filter.as_deref() == Some(old.0.as_str()) {
            self.active_saved_filter = Some(name.clone());
        }
        self.set_status(format!("saved filter '{}': {}", name, old.1));
    }

    /// Row indices of actionable items (headers aren't selectable).
    pub(crate) fn settings_item_rows(&self) -> Vec<usize> {
        self.settings_rows()
            .iter()
            .enumerate()
            .filter(|(_, r)| matches!(r, SettingRow::Item { .. }))
            .map(|(i, _)| i)
            .collect()
    }

    /// Settings sections, derived from the row list: each `Header`
    /// starts a section carrying the rows beneath it. One source of
    /// truth — the sidebar and the content pane split the same rows.
    /// Returns `(name, content row indices)` per section.
    pub fn settings_sections(&self) -> Vec<(&'static str, Vec<usize>)> {
        let mut out: Vec<(&'static str, Vec<usize>)> = Vec::new();
        for (i, r) in self.settings_rows().iter().enumerate() {
            if let SettingRow::Header(name) = r {
                out.push((name, Vec::new()));
            } else if let Some((_, rows)) = out.last_mut() {
                rows.push(i);
            }
        }
        out
    }

    /// Header row of a section: headers delimit contiguous spans, so it
    /// sits immediately before the section's first content row.
    pub(crate) fn settings_section_header(
        sections: &[(&'static str, Vec<usize>)],
        sec: usize,
    ) -> usize {
        sections
            .get(sec)
            .and_then(|(_, rows)| rows.first())
            .copied()
            .unwrap_or(1)
            .saturating_sub(1)
    }

    /// Section index holding a global settings row (headers belong to
    /// the section they start).
    pub(crate) fn settings_section_of_row(&self, row: usize) -> usize {
        let sections = self.settings_sections();
        let mut sec = 0;
        for (i, _) in sections.iter().enumerate() {
            if Self::settings_section_header(&sections, i) <= row {
                sec = i;
            } else {
                break;
            }
        }
        sec
    }

    /// Snap the displayed section + scroll to the selected item: the
    /// sidebar follows the selection, and the scroll lands on the
    /// section's first content row (the header itself lives in the
    /// sidebar/title, not the content pane).
    pub(crate) fn snap_settings_to_selection(&mut self) {
        let items = self.settings_item_rows();
        let row = items.get(self.settings_selected).copied().unwrap_or(0);
        let sections = self.settings_sections();
        self.settings_section = self.settings_section_of_row(row);
        self.settings_scroll = sections
            .get(self.settings_section)
            .and_then(|(_, rows)| rows.first())
            .copied()
            .unwrap_or(0);
    }

    /// Theme + density the settings preview renders with: the hovered
    /// option when standing on Theme/Density (browse = preview), else
    /// the applied config. Lets an empty setup try styles against
    /// sample rows instead of a blank table.
    pub fn settings_preview_style(&self) -> (String, bool) {
        use crate::config::{DENSITIES, THEMES};
        let items = self.settings_item_rows();
        let rows = self.settings_rows();
        let mut theme = self.config.theme.clone();
        let mut comfortable = !self.config.compact();
        if let Some(&row) = items.get(self.settings_selected)
            && let Some(SettingRow::Item { kind, .. }) = rows.get(row)
        {
            match kind {
                SettingKind::Theme(i) => {
                    theme = THEMES.get(*i).unwrap_or(&theme.as_str()).to_string();
                }
                SettingKind::Density(i) => {
                    comfortable = DENSITIES
                        .get(*i)
                        .map(|d| *d == "comfortable")
                        .unwrap_or(comfortable);
                }
                _ => {}
            }
        }
        (theme, comfortable)
    }

    /// Jump to a section (Left/Right, sidebar clicks): show it, select
    /// its first item when it has one, snap scroll to its first content
    /// row. Item-less sections (the tray-icon legend) keep the stored
    /// selection and just show.
    pub fn goto_settings_section(&mut self, sec: usize) {
        let sections = self.settings_sections();
        if sections.is_empty() {
            return;
        }
        let next = sec.min(sections.len() - 1);
        self.settings_section = next;
        // First *selectable* row of the section (headers aren't).
        let items = self.settings_item_rows();
        if let Some(p) = sections[next]
            .1
            .iter()
            .filter_map(|&r| items.iter().position(|&ir| ir == r))
            .next()
        {
            self.settings_selected = p;
        }
        if let Some(&first) = sections[next].1.first() {
            self.settings_scroll = first;
        }
    }

    /// Jump to the neighbouring section, wrapping around.
    pub fn move_settings_section(&mut self, dir: i32) {
        let n = self.settings_sections().len();
        if n == 0 {
            return;
        }
        let cur = self.settings_section.min(n - 1);
        self.goto_settings_section((cur as i32 + dir).rem_euclid(n as i32) as usize);
    }

    pub fn open_settings(&mut self) {
        self.mode = Mode::Settings;
        self.settings_selected = 0;
        self.settings_scroll = 0;
        self.settings_section = 0;
    }

    /// Apply the item at popup row `row` (for mouse clicks).
    pub fn settings_apply_at_row(&mut self, row: usize) -> bool {
        let kind = self.settings_rows().get(row).and_then(|r| match r {
            SettingRow::Item { kind, .. } => Some(*kind),
            SettingRow::Header(_) | SettingRow::Legend { .. } => None,
        });
        match kind {
            Some(k) => {
                self.apply_setting(k);
                true
            }
            None => false,
        }
    }

    /// Apply a setting live and persist it to the config file.
    pub fn apply_setting(&mut self, kind: SettingKind) {
        use crate::config::{DENSITIES, THEMES};
        let label = match kind {
            SettingKind::Theme(i) => {
                self.config.theme = THEMES[i % THEMES.len()].to_string();
                self.view_changes.theme = true;
                format!("theme: {}", self.config.theme)
            }
            SettingKind::Density(i) => {
                self.config.density = DENSITIES[i % DENSITIES.len()].to_string();
                self.view_changes.density = true;
                format!("density: {}", self.config.density)
            }
            SettingKind::ToggleNotifications => {
                self.config.notifications_enabled = !self.config.notifications_enabled;
                self.view_changes.notifications = true;
                format!(
                    "notifications: {}",
                    Self::onoff(self.config.notifications_enabled)
                )
            }
            SettingKind::ToggleHighlight => {
                self.config.highlight_attached = !self.config.highlight_attached;
                self.view_changes.highlight = true;
                format!(
                    "highlight attached: {}",
                    Self::onoff(self.config.highlight_attached)
                )
            }
            SettingKind::ToggleMruTabs => {
                self.config.mru_tabs = !self.config.mru_tabs;
                self.view_changes.mru_tabs = true;
                self.apply_tab_order();
                format!("mru tabs: {}", Self::onoff(self.config.mru_tabs))
            }
            SettingKind::ToggleBlurCols => {
                self.config.blur_cols = !self.config.blur_cols;
                // Only the blur key changed: flagging density too made
                // a stale second instance rewrite its stale density
                // over a newer instance's saved value.
                self.view_changes.blur_cols = true;
                format!("blur columns: {}", Self::onoff(self.config.blur_cols))
            }
            SettingKind::EditStandingFilter => {
                // Editing, not applying: the prompt saves on Enter.
                // Return early so the tail below doesn't persist
                // unrelated settings or claim anything was saved.
                self.open_standing_editor();
                return;
            }
            SettingKind::UseSavedFilter(i) => {
                // Applying a saved filter is not a settings write: the
                // list itself is unchanged, so return before the tail
                // (which both persists and claims "saved").
                self.use_saved_filter(i);
                return;
            }
            SettingKind::AddSavedFilter => {
                self.open_saved_filter_prompt(None);
                return;
            }
            SettingKind::RenameSavedFilter(i) => {
                if i < self.saved_filters.len() {
                    self.open_saved_filter_prompt(Some(i));
                }
                return;
            }
            SettingKind::DeleteSavedFilter(i) => {
                if let Some(f) = self.saved_filters.get(i) {
                    let name = f.name.clone();
                    self.delete_saved_filter(i);
                    self.set_status(format!("deleted filter '{name}'"));
                }
                return;
            }
        };
        // Only the keys this arm touched reach the file: writing the
        // whole snapshot here is what let a stale instance's toggle
        // revert a newer instance's filter (see `App::persist_view`).
        let written = {
            let patch = self.config.ui_persist_changes(&self.view_changes);
            crate::config::persist_ui_settings(&patch)
        };
        match written {
            Ok(()) => {
                self.view_changes = crate::config::ViewChanges::default();
                self.set_status(format!("{label} (saved)"));
            }
            Err(e) => self.set_status(format!("{label} (live only: {e})")),
        }
    }

    pub(crate) fn move_settings_up(&mut self) {
        let before = self.settings_selected;
        self.settings_selected = self.settings_selected.saturating_sub(1);
        if self.settings_selected != before {
            self.snap_settings_to_selection();
        }
    }

    pub(crate) fn move_settings_down(&mut self) {
        let before = self.settings_selected;
        if self.settings_selected + 1 < self.settings_item_rows().len() {
            self.settings_selected += 1;
        }
        if self.settings_selected != before {
            self.snap_settings_to_selection();
        }
    }
}
