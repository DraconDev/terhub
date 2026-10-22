//! The working-tree summary the preview title shows.
//!
//! Git is read through `--no-optional-locks`, so a read-only probe
//! never blocks on a lock a running agent holds.

/// Working-tree summary for the preview title.
pub(crate) struct GitStat {
    pub(crate) branch: String,
    pub(crate) files: u32,
    pub(crate) ins: u32,
    pub(crate) del: u32,
    pub(crate) untracked: u32,
}

impl GitStat {
    pub(crate) fn render(&self) -> String {
        let mut s = format!("git {}", self.branch);
        if self.files == 0 && self.untracked == 0 {
            return s + " · clean";
        }
        s.push_str(&format!(" +{}/-{}", self.ins, self.del));
        if self.files > 0 {
            s.push_str(&format!(" · {} changed", self.files));
        }
        if self.untracked > 0 {
            s.push_str(&format!(" · {} new", self.untracked));
        }
        s
    }
}

/// Branch + diff + untracked count for a dir (None outside a repo).
/// `--no-optional-locks` keeps read-only probes from ever blocking on
/// a lock a running agent holds.
pub(crate) fn git_diff_stat(dir: &str) -> Option<GitStat> {
    let branch = git_out(dir, &["branch", "--show-current"])?;
    let branch = branch.trim();
    let branch = if branch.is_empty() {
        "detached".to_string()
    } else {
        branch.to_string()
    };
    let short = git_out(dir, &["diff", "--shortstat"])?;
    let (files, ins, del) = parse_shortstat(&short);
    let status = git_out(
        dir,
        &["status", "--porcelain=v1", "--untracked-files=normal"],
    )?;
    let untracked = status.lines().filter(|l| l.starts_with("??")).count() as u32;
    Some(GitStat {
        branch,
        files,
        ins,
        del,
        untracked,
    })
}

fn git_out(dir: &str, args: &[&str]) -> Option<String> {
    let out = std::process::Command::new("git")
        .arg("-C")
        .arg(dir)
        .arg("--no-optional-locks")
        .args(args)
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    Some(String::from_utf8_lossy(&out.stdout).into_owned())
}

/// Parse `git diff --shortstat`: any part may be absent when zero, and
/// empty output means a clean tree.
pub(crate) fn parse_shortstat(out: &str) -> (u32, u32, u32) {
    let mut files = 0;
    let mut ins = 0;
    let mut del = 0;
    for part in out.split(',') {
        let num: u32 = part
            .split_whitespace()
            .next()
            .and_then(|n| n.parse().ok())
            .unwrap_or(0);
        if part.contains("file") {
            files = num;
        } else if part.contains("insertion") {
            ins = num;
        } else if part.contains("deletion") {
            del = num;
        }
    }
    (files, ins, del)
}
