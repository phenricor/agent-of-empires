//! Discover nested git repositories below a launch directory.
//!
//! Powers `aoe add --scan`: walk the launch dir, collect every repo root, and
//! place a worktree for each while preserving its relative path (mirrors
//! claude-squad's `-g` group scanning). A repo root is a directory containing a
//! `.git` *directory*; a `.git` *file* marks a linked worktree or a submodule
//! pointer, which we skip so those aren't mistaken for standalone repos. We
//! never descend into a nested repo we found, so submodules stay part of their
//! parent. The launch dir is the one exception: when it is itself a repo root it
//! joins the results *and* we keep descending, because a monorepo root whose
//! subprojects are separate repos (orchestration files at the top, code in
//! `src/**`) needs both halves in the workspace.

use std::path::{Path, PathBuf};

/// Caps how deep the scan descends below the launch dir.
const MAX_DEPTH: usize = 8;

/// Directory names never descended into while scanning.
const PRUNED_DIRS: &[&str] = &[
    ".git",
    "node_modules",
    "bin",
    "obj",
    "packages",
    ".vs",
    ".idea",
    "target",
];

/// True when `dir` is a repo root: it contains a `.git` directory. A `.git`
/// file (linked worktree / submodule pointer) does not count.
fn is_repo_root(dir: &Path) -> bool {
    dir.join(".git").is_dir()
}

/// Find every git repo at or below `launch_dir`, up to `MAX_DEPTH`. Returns
/// absolute paths sorted for a stable order, so `launch_dir` (a prefix of the
/// rest) comes first when it is a repo. Nested repos found are not descended
/// into; `launch_dir` is, so a monorepo root and its subproject repos are all
/// reported.
pub fn scan_nested_repos(launch_dir: &Path) -> Vec<PathBuf> {
    let launch_abs = launch_dir
        .canonicalize()
        .unwrap_or_else(|_| launch_dir.to_path_buf());
    let mut found = Vec::new();
    if is_repo_root(&launch_abs) {
        found.push(launch_abs.clone());
    }
    walk(&launch_abs, 0, &mut found);
    found.sort();
    found
}

fn walk(dir: &Path, depth: usize, found: &mut Vec<PathBuf>) {
    if depth > MAX_DEPTH {
        return;
    }
    let entries = match std::fs::read_dir(dir) {
        Ok(e) => e,
        Err(_) => return,
    };
    for entry in entries.flatten() {
        if !entry.file_type().map(|t| t.is_dir()).unwrap_or(false) {
            continue;
        }
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if PRUNED_DIRS.contains(&name.as_ref()) {
            continue;
        }
        let child = entry.path();
        if is_repo_root(&child) {
            // Record the repo and stop; don't treat its submodules as separate.
            found.push(child);
            continue;
        }
        walk(&child, depth + 1, found);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn init_repo(path: &Path) {
        std::fs::create_dir_all(path).unwrap();
        std::process::Command::new("git")
            .args(["init", "-q"])
            .current_dir(path)
            .status()
            .unwrap();
    }

    #[test]
    fn finds_nested_repos_and_preserves_paths() {
        let root = tempfile::tempdir().unwrap();
        let base = root.path();
        init_repo(&base.join("src/Core/RepoA"));
        init_repo(&base.join("src/Jobs/RepoB"));
        // A plain (non-repo) directory is skipped.
        std::fs::create_dir_all(base.join("src/Empty")).unwrap();

        let repos = scan_nested_repos(base);
        let rel: Vec<String> = repos
            .iter()
            .map(|p| {
                p.strip_prefix(base.canonicalize().unwrap())
                    .unwrap()
                    .to_string_lossy()
                    .to_string()
            })
            .collect();
        assert_eq!(rel, vec!["src/Core/RepoA", "src/Jobs/RepoB"]);
    }

    #[test]
    fn includes_launch_dir_when_it_is_a_repo_root() {
        let root = tempfile::tempdir().unwrap();
        let base = root.path();
        init_repo(base);
        init_repo(&base.join("src/Core/RepoA"));

        let repos = scan_nested_repos(base);
        let canon = base.canonicalize().unwrap();
        assert_eq!(
            repos,
            vec![canon.clone(), canon.join("src/Core/RepoA")],
            "the monorepo root sorts first, its subproject repo follows"
        );
    }

    #[test]
    fn skips_launch_dir_when_it_is_a_linked_worktree() {
        let root = tempfile::tempdir().unwrap();
        let base = root.path();
        // A linked worktree has a `.git` FILE, so it is not a repo root.
        std::fs::create_dir_all(base).unwrap();
        std::fs::write(base.join(".git"), "gitdir: /elsewhere/.git/worktrees/wt\n").unwrap();
        init_repo(&base.join("nested"));

        let repos = scan_nested_repos(base);
        assert_eq!(repos.len(), 1);
        assert!(repos[0].ends_with("nested"));
    }

    #[test]
    fn does_not_descend_into_found_repo() {
        let root = tempfile::tempdir().unwrap();
        let base = root.path();
        init_repo(&base.join("outer"));
        // A repo nested inside another repo must not be reported separately.
        init_repo(&base.join("outer/inner"));

        let repos = scan_nested_repos(base);
        assert_eq!(repos.len(), 1, "should stop at the outer repo");
        assert!(repos[0].ends_with("outer"));
    }

    #[test]
    fn prunes_ignored_dirs() {
        let root = tempfile::tempdir().unwrap();
        let base = root.path();
        init_repo(&base.join("node_modules/pkg"));
        init_repo(&base.join("keep"));

        let repos = scan_nested_repos(base);
        assert_eq!(repos.len(), 1);
        assert!(repos[0].ends_with("keep"));
    }
}
