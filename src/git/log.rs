//! Session commit log: the commits a session added on top of its base.

use std::path::Path;

use super::diff::get_commit_from_ref;
use super::error::Result;

/// One commit on the session branch, formatted for display.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommitInfo {
    pub short_hash: String,
    pub subject: String,
    pub author: String,
    /// Author date as `YYYY-MM-DD`.
    pub date: String,
}

/// Commits reachable from HEAD but not from the base branch, newest first.
///
/// Hides the merge-base rather than the base tip, so commits that landed on the
/// base branch after this session started are not reported as the session's
/// (the same rule the diff view uses).
pub fn commits_since_base(repo_path: &Path, base_branch: &str) -> Result<Vec<CommitInfo>> {
    let repo = super::open_repo_at(repo_path)?;
    let head = repo.head()?.peel_to_commit()?;

    let mut walk = repo.revwalk()?;
    walk.push(head.id())?;
    walk.set_sorting(git2::Sort::TIME)?;

    // An unresolvable base (branch missing in this repo) leaves the walk
    // unbounded, so fall back to hiding nothing and cap the output instead.
    if let Ok(base) = get_commit_from_ref(&repo, base_branch) {
        let stop = repo
            .merge_base(head.id(), base.id())
            .unwrap_or_else(|_| base.id());
        // Hiding a commit that isn't an ancestor is not an error for git2; it
        // simply prunes nothing.
        let _ = walk.hide(stop);
    }

    let mut commits = Vec::new();
    for oid in walk {
        let commit = repo.find_commit(oid?)?;
        commits.push(CommitInfo {
            short_hash: commit.id().to_string().chars().take(8).collect(),
            subject: commit.summary().ok().flatten().unwrap_or("").to_string(),
            author: commit.author().name().unwrap_or("").to_string(),
            date: format_date(commit.time().seconds()),
        });
        // Guard against an unbounded walk when the base could not be resolved.
        if commits.len() >= MAX_COMMITS {
            break;
        }
    }
    Ok(commits)
}

/// Upper bound on reported commits, so an unresolvable base cannot walk an
/// entire repository's history into the view.
const MAX_COMMITS: usize = 500;

fn format_date(epoch_seconds: i64) -> String {
    chrono::DateTime::from_timestamp(epoch_seconds, 0)
        .map(|dt| dt.format("%Y-%m-%d").to_string())
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn git(dir: &Path, args: &[&str]) {
        let ok = std::process::Command::new("git")
            .args(["-C", dir.to_str().unwrap()])
            .args(args)
            .status()
            .unwrap()
            .success();
        assert!(ok, "git {args:?} failed");
    }

    /// Repo on `main` with one base commit, then `extra` commits on a session
    /// branch on top of it.
    fn repo_with_session_commits(extra: usize) -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path();
        git(p, &["init", "-q", "-b", "main"]);
        git(p, &["config", "user.email", "t@t"]);
        git(p, &["config", "user.name", "Tester"]);
        std::fs::write(p.join("base.txt"), "base\n").unwrap();
        git(p, &["add", "."]);
        git(p, &["commit", "-q", "-m", "base commit"]);
        git(p, &["checkout", "-q", "-b", "session"]);
        for i in 0..extra {
            std::fs::write(p.join(format!("f{i}.txt")), "x\n").unwrap();
            git(p, &["add", "."]);
            git(p, &["commit", "-q", "-m", &format!("session change {i}")]);
        }
        dir
    }

    #[test]
    fn lists_only_commits_after_the_base() {
        let repo = repo_with_session_commits(2);
        let commits = commits_since_base(repo.path(), "main").unwrap();

        assert_eq!(commits.len(), 2, "base commit must be excluded");
        // Newest first.
        assert_eq!(commits[0].subject, "session change 1");
        assert_eq!(commits[1].subject, "session change 0");
        assert_eq!(commits[0].author, "Tester");
        assert_eq!(commits[0].short_hash.len(), 8);
        assert_eq!(commits[0].date.len(), 10, "date should be YYYY-MM-DD");
    }

    #[test]
    fn no_session_commits_yields_empty() {
        let repo = repo_with_session_commits(0);
        assert!(commits_since_base(repo.path(), "main").unwrap().is_empty());
    }
}
