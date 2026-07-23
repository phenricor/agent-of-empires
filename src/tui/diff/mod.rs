//! Diff view - view changes against a base branch

mod highlight;
mod input;
mod render;
mod split;

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

use crate::file_watch::FileWatchService;
use crate::git::diff::{
    check_merge_base_status, compute_changed_files, compute_file_diff, get_default_base_ref,
    list_branches, DiffFile, FileDiff,
};
use crate::session::config::{update_app_state, update_config};
use crate::session::{load_profile_config, resolve_config_or_warn, save_profile_config, Config};
use crate::tui::dialogs::InfoDialog;

pub use input::DiffAction;

/// State for branch selection dialog
#[derive(Debug, Clone, Default)]
pub struct BranchSelectState {
    pub branches: Vec<String>,
    pub selected: usize,
}

/// One repository the diff view aggregates over. Single-repo sessions have one;
/// multi-repo workspace sessions have one per member repo, so the file list and
/// per-file diffs span every repo (like claude-squad's group diff).
#[derive(Debug, Clone)]
pub struct DiffRepo {
    /// Display name (repo directory basename), shown as a prefix when there is
    /// more than one repo.
    pub name: String,
    /// Worktree root this repo's files and diffs are computed against.
    pub root: PathBuf,
}

/// The diff view state
pub struct DiffView {
    /// Path to the primary repository root. For a workspace this is the first
    /// member repo; branch listing and merge-base status use it.
    pub(crate) repo_path: PathBuf,

    /// Every repo the view aggregates over (one for a single-repo session,
    /// one per member for a workspace). `files[i]` belongs to
    /// `repos[file_repo_idx[i]]`.
    pub(crate) repos: Vec<DiffRepo>,

    /// Repo index for each entry in `files`, kept in lockstep with it.
    pub(crate) file_repo_idx: Vec<usize>,

    /// Session id this diff view belongs to. None when opened in a
    /// session-agnostic context (legacy `DiffView::new`); persistence
    /// of the per-session base-branch override is skipped in that case.
    pub(crate) session_id: Option<String>,

    /// Profile the session belongs to, used to look up `Storage` when
    /// persisting the base-branch override.
    pub(crate) profile: String,

    /// Base branch to compare against
    pub(crate) base_branch: String,

    /// List of changed files
    pub(crate) files: Vec<DiffFile>,

    /// Currently selected file index
    pub(crate) selected_file: usize,

    /// Cached file diffs
    pub(crate) diff_cache: HashMap<PathBuf, FileDiff>,

    /// Scroll offset for the diff content
    pub(crate) scroll_offset: u16,

    /// Number of visible lines (set during render)
    pub(crate) visible_lines: u16,

    /// Total lines in current diff
    pub(crate) total_lines: u16,

    /// Branch selection dialog state
    pub(crate) branch_select: Option<BranchSelectState>,

    /// Error message to display
    pub(crate) error_message: Option<String>,

    /// Success message to display
    pub(crate) success_message: Option<String>,

    /// Context lines for diff
    pub(crate) context_lines: usize,

    /// Render the selected file's diff side-by-side instead of unified.
    pub(crate) split_view: bool,

    /// Show help overlay
    pub(crate) show_help: bool,

    /// Width of the file list panel (resizable with h/l)
    pub(crate) file_list_width: u16,

    /// Warning dialog shown when merge-base can't be computed
    pub(crate) warning_dialog: Option<InfoDialog>,

    /// Override that has been persisted to disk but not yet propagated
    /// back to HomeView's in-memory `Instance.base_branch_override`.
    /// HomeView consumes this after each key event via
    /// `take_pending_override` and applies it to its cache; without
    /// this hand-off, HomeView's next `commit` would overwrite the
    /// disk value with its stale memory copy. See #1175.
    pub(crate) pending_override: Option<(String, Option<String>)>,

    /// Inner rect of the file-list panel, captured during `render`.
    /// Lets a click on a file row select it and a hover highlight it
    /// the same way `j`/`k` would.
    pub(crate) file_list_inner: ratatui::layout::Rect,

    /// First file index currently rendered in the file-list panel.
    /// Keeps keyboard selection and mouse clicks aligned when the file list is
    /// taller than the visible panel.
    pub(crate) file_list_scroll_offset: usize,

    /// Process-wide file-watch primitive, threaded through to per-session
    /// `Storage` writes so the local in-process Local fast path fires when
    /// the diff view persists a `base_branch_override`.
    pub(crate) file_watch: Arc<FileWatchService>,
}

impl DiffView {
    /// Create a session-agnostic diff view. Selecting a different
    /// branch through the picker only mutates in-memory state. Callers
    /// that have a session id should use `new_for_session` so the
    /// override persists.
    pub fn new(repo_path: PathBuf, file_watch: Arc<FileWatchService>) -> anyhow::Result<Self> {
        Self::new_for_session(
            repo_path,
            Vec::new(),
            None,
            String::new(),
            None,
            None,
            file_watch,
        )
    }

    /// Create a diff view bound to a session. `base_override` (the
    /// session's persisted `base_branch_override`) wins over the
    /// worktree's recorded base branch (`worktree_base`), which wins
    /// over the profile default and auto-detection. Subsequent calls
    /// to `select_branch` persist the new ref back to the session
    /// record.
    pub fn new_for_session(
        repo_path: PathBuf,
        workspace_repos: Vec<DiffRepo>,
        session_id: Option<String>,
        profile: String,
        base_override: Option<String>,
        worktree_base: Option<String>,
        file_watch: Arc<FileWatchService>,
    ) -> anyhow::Result<Self> {
        // A workspace session aggregates over its member repos; a plain session
        // is a single repo at `repo_path`. Either way `repos[0]` is the primary
        // used for branch listing / merge-base, and `repo_path` follows it.
        let repos = if workspace_repos.is_empty() {
            vec![DiffRepo {
                name: repo_path
                    .file_name()
                    .map(|n| n.to_string_lossy().to_string())
                    .unwrap_or_else(|| "repo".to_string()),
                root: repo_path.clone(),
            }]
        } else {
            workspace_repos
        };
        let repo_path = repos[0].root.clone();
        // Use the profile-merged config so a per-profile Diff override (e.g.
        // split_view) is honored on open. The session-agnostic path (empty
        // profile) falls back to the global config.
        let config = if profile.is_empty() {
            Config::load_or_warn()
        } else {
            resolve_config_or_warn(&profile)
        };

        let base_branch = base_override
            .as_deref()
            .map(str::trim)
            .filter(|v| !v.is_empty())
            .map(str::to_string)
            .or_else(|| {
                worktree_base
                    .as_deref()
                    .map(str::trim)
                    .filter(|v| !v.is_empty())
                    .map(str::to_string)
            })
            .or_else(|| config.diff.default_branch.clone())
            .or_else(|| get_default_base_ref(&repo_path).ok())
            .unwrap_or_else(|| "main".to_string());

        let context_lines = config.diff.context_lines;
        let split_view = config.diff.split_view;

        let warning_dialog = check_merge_base_status(&repo_path, &base_branch)
            .map(|msg| InfoDialog::new("Warning", &msg));

        let mut view = Self {
            repo_path,
            repos,
            file_repo_idx: Vec::new(),
            session_id,
            profile,
            base_branch,
            files: Vec::new(),
            selected_file: 0,
            diff_cache: HashMap::new(),
            scroll_offset: 0,
            visible_lines: 20,
            total_lines: 0,
            branch_select: None,
            error_message: None,
            success_message: None,
            context_lines,
            split_view,
            show_help: false,
            file_list_width: config.app_state.diff_file_list_width.unwrap_or(35),
            warning_dialog,
            pending_override: None,
            file_list_inner: ratatui::layout::Rect::default(),
            file_list_scroll_offset: 0,
            file_watch,
        };

        view.refresh_files()?;
        Ok(view)
    }

    /// Repo root the file at `idx` belongs to.
    fn file_root(&self, idx: usize) -> &std::path::Path {
        let repo = self.file_repo_idx.get(idx).copied().unwrap_or(0);
        &self.repos[repo.min(self.repos.len().saturating_sub(1))].root
    }

    /// Cache/lookup key for a file's diff. Uses the file's worktree-absolute
    /// path so two member repos sharing a relative path (e.g. `README.md`) do
    /// not collide.
    pub(crate) fn diff_key(&self, idx: usize) -> PathBuf {
        match self.files.get(idx) {
            Some(f) => self.file_root(idx).join(&f.path),
            None => PathBuf::new(),
        }
    }

    /// Repo display name for the file at `idx`, or None when the view spans a
    /// single repo (nothing to disambiguate).
    pub(crate) fn file_repo_name(&self, idx: usize) -> Option<&str> {
        if self.repos.len() < 2 {
            return None;
        }
        let repo = self.file_repo_idx.get(idx).copied().unwrap_or(0);
        self.repos.get(repo).map(|r| r.name.as_str())
    }

    /// Refresh the list of changed files, aggregating across every repo. A repo
    /// that errors (e.g. the base ref is missing there) is skipped rather than
    /// failing the whole view.
    pub fn refresh_files(&mut self) -> anyhow::Result<()> {
        let mut files = Vec::new();
        let mut file_repo_idx = Vec::new();
        for (i, repo) in self.repos.iter().enumerate() {
            match compute_changed_files(&repo.root, &self.base_branch) {
                Ok(fs) => {
                    for f in fs {
                        files.push(f);
                        file_repo_idx.push(i);
                    }
                }
                Err(e) => {
                    tracing::debug!(target: "tui.diff", "skip repo {} in diff: {e}", repo.name);
                }
            }
        }
        self.files = files;
        self.file_repo_idx = file_repo_idx;
        self.diff_cache.clear();
        if self.selected_file >= self.files.len() {
            self.selected_file = self.files.len().saturating_sub(1);
        }
        if self.files.is_empty() {
            self.file_list_scroll_offset = 0;
        } else {
            self.file_list_scroll_offset = self
                .file_list_scroll_offset
                .min(self.files.len().saturating_sub(1));
        }
        self.scroll_offset = 0;
        Ok(())
    }

    /// Get the currently selected file
    pub fn selected_file(&self) -> Option<&DiffFile> {
        self.files.get(self.selected_file)
    }

    /// Repo-relative path of the selected file as a string, if any.
    pub(crate) fn selected_path_string(&self) -> Option<String> {
        self.selected_file()
            .map(|f| f.path.to_string_lossy().to_string())
    }

    /// Copy the selected file's repo-relative path to the system clipboard and
    /// surface a confirmation in the footer.
    pub(crate) fn copy_selected_path(&mut self) {
        if let Some(path) = self.selected_path_string() {
            crate::tui::clipboard::copy_to_clipboard(&path);
            self.success_message = Some(format!("Copied {path}"));
        }
    }

    /// Get or compute the diff for the selected file
    pub fn get_current_diff(&mut self) -> Option<&FileDiff> {
        let idx = self.selected_file;
        let file = self.files.get(idx)?;
        let rel_path = file.path.clone();
        let root = self.file_root(idx).to_path_buf();
        let key = self.diff_key(idx);

        if !self.diff_cache.contains_key(&key) {
            match compute_file_diff(&root, &rel_path, &self.base_branch, self.context_lines) {
                Ok(diff) => {
                    self.diff_cache.insert(key.clone(), diff);
                }
                Err(e) => {
                    self.error_message = Some(format!("Failed to compute diff: {}", e));
                    return None;
                }
            }
        }

        self.diff_cache.get(&key)
    }

    /// Open the branch selection dialog
    pub fn open_branch_select(&mut self) {
        match list_branches(&self.repo_path) {
            Ok(branches) => {
                let selected = branches
                    .iter()
                    .position(|b| b == &self.base_branch)
                    .unwrap_or(0);
                self.branch_select = Some(BranchSelectState { branches, selected });
            }
            Err(e) => {
                self.error_message = Some(format!("Failed to list branches: {}", e));
            }
        }
    }

    /// Select a branch and refresh. When the view is bound to a
    /// session, the choice is persisted as `base_branch_override` on
    /// the session record so the next launch comes back to the same
    /// comparison. Persistence failures only surface as a soft error
    /// message; the in-memory switch still applies. See #970.
    pub fn select_branch(&mut self, branch: String) {
        self.base_branch = branch;
        self.branch_select = None;
        self.warning_dialog = check_merge_base_status(&self.repo_path, &self.base_branch)
            .map(|msg| InfoDialog::new("Warning", &msg));
        if let Err(e) = self.persist_base_override() {
            self.error_message = Some(format!("Failed to persist base branch: {e}"));
        }
        if let Err(e) = self.refresh_files() {
            self.error_message = Some(format!("Failed to refresh: {}", e));
        }
    }

    fn persist_base_override(&mut self) -> anyhow::Result<()> {
        let Some(session_id) = self.session_id.clone() else {
            return Ok(());
        };
        let storage = crate::session::Storage::new(&self.profile, self.file_watch.clone())?;
        let new_override = Some(self.base_branch.clone());
        let id_for_closure = session_id.clone();
        let new_override_for_closure = new_override.clone();
        storage.update(|instances, _groups| {
            if let Some(inst) = instances.iter_mut().find(|i| i.id == id_for_closure) {
                inst.base_branch_override = new_override_for_closure;
            }
            Ok(())
        })?;
        self.pending_override = Some((session_id, new_override));
        Ok(())
    }

    /// Drain a pending base-branch override that was just persisted to
    /// disk. HomeView calls this after each key event so its in-memory
    /// `Instance.base_branch_override` stays consistent with disk;
    /// otherwise its next `commit` would overwrite the persisted value.
    pub fn take_pending_override(&mut self) -> Option<(String, Option<String>)> {
        self.pending_override.take()
    }

    /// Navigate to next file
    pub fn next_file(&mut self) {
        if self.selected_file < self.files.len().saturating_sub(1) {
            self.selected_file += 1;
            self.scroll_offset = 0;
        }
    }

    /// Navigate to previous file
    pub fn prev_file(&mut self) {
        if self.selected_file > 0 {
            self.selected_file -= 1;
            self.scroll_offset = 0;
        }
    }

    /// Keep the selected file within the visible file-list rows.
    pub(crate) fn ensure_selected_file_visible_in_list(&mut self, visible_rows: usize) {
        if self.files.is_empty() || visible_rows == 0 {
            self.file_list_scroll_offset = 0;
            return;
        }

        let selected = self.selected_file.min(self.files.len().saturating_sub(1));
        let max_offset = self.files.len().saturating_sub(visible_rows);
        self.file_list_scroll_offset = self.file_list_scroll_offset.min(max_offset);

        if selected < self.file_list_scroll_offset {
            self.file_list_scroll_offset = selected;
        } else if selected >= self.file_list_scroll_offset + visible_rows {
            self.file_list_scroll_offset = selected + 1 - visible_rows;
        }

        self.file_list_scroll_offset = self.file_list_scroll_offset.min(max_offset);
    }

    /// Scroll diff content down
    pub fn scroll_down(&mut self, amount: u16) {
        let max_scroll = self.total_lines.saturating_sub(self.visible_lines);
        self.scroll_offset = (self.scroll_offset + amount).min(max_scroll);
    }

    /// Scroll diff content up
    pub fn scroll_up(&mut self, amount: u16) {
        self.scroll_offset = self.scroll_offset.saturating_sub(amount);
    }

    /// Page down in diff content
    pub fn page_down(&mut self) {
        self.scroll_down(self.visible_lines.saturating_sub(2));
    }

    /// Page up in diff content
    pub fn page_up(&mut self) {
        self.scroll_up(self.visible_lines.saturating_sub(2));
    }

    /// Half-page down in diff content
    pub fn half_page_down(&mut self) {
        self.scroll_down(self.visible_lines / 2);
    }

    /// Half-page up in diff content
    pub fn half_page_up(&mut self) {
        self.scroll_up(self.visible_lines / 2);
    }

    /// Shrink the file list panel
    pub fn shrink_file_list(&mut self) {
        self.file_list_width = self.file_list_width.saturating_sub(5).max(5);
        self.save_file_list_width();
    }

    /// Grow the file list panel
    pub fn grow_file_list(&mut self) {
        self.file_list_width = (self.file_list_width + 5).min(80);
        self.save_file_list_width();
    }

    fn save_file_list_width(&self) {
        let _ = update_app_state(|state| {
            state.diff_file_list_width = Some(self.file_list_width);
        });
    }

    /// Persist the current `split_view` choice so it survives restarts and
    /// stays in sync with the settings TUI. A profile-scoped session writes the
    /// choice to that profile's override; a session-agnostic view writes the
    /// global default.
    pub(crate) fn persist_split_view(&self) {
        if self.profile.is_empty() {
            let split_view = self.split_view;
            if let Err(e) = update_config(|config| {
                config.diff.split_view = split_view;
            }) {
                tracing::warn!("failed to persist diff split_view: {e}");
            }
            return;
        }
        match load_profile_config(&self.profile) {
            Ok(mut profile_config) => {
                // Overrides are sparse JSON (#1692): set diff.split_view in the
                // generic override map rather than a typed DiffConfigOverride.
                let diff = profile_config
                    .overrides
                    .entry("diff".to_string())
                    .or_insert_with(|| serde_json::json!({}));
                if let Some(obj) = diff.as_object_mut() {
                    obj.insert("split_view".to_string(), serde_json::json!(self.split_view));
                }
                if let Err(e) = save_profile_config(&self.profile, &profile_config) {
                    tracing::warn!(
                        "failed to persist diff split_view for profile {}: {e}",
                        self.profile
                    );
                }
            }
            Err(e) => {
                tracing::warn!("failed to load profile config {}: {e}", self.profile);
            }
        }
    }

    /// Minimal DiffView for unit tests. Centralised so new fields only need
    /// a default value in one place.
    #[cfg(test)]
    pub(crate) fn test_default() -> Self {
        Self {
            repo_path: std::path::PathBuf::from("/tmp/fake"),
            repos: vec![DiffRepo {
                name: "fake".to_string(),
                root: std::path::PathBuf::from("/tmp/fake"),
            }],
            file_repo_idx: Vec::new(),
            session_id: None,
            profile: String::new(),
            base_branch: "main".to_string(),
            files: Vec::new(),
            selected_file: 0,
            diff_cache: HashMap::new(),
            scroll_offset: 0,
            visible_lines: 20,
            total_lines: 0,
            branch_select: None,
            error_message: None,
            success_message: None,
            context_lines: 3,
            split_view: false,
            show_help: false,
            file_list_width: 35,
            warning_dialog: None,
            pending_override: None,
            file_list_inner: ratatui::layout::Rect::default(),
            file_list_scroll_offset: 0,
            file_watch: FileWatchService::noop(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    fn git(dir: &Path, args: &[&str]) {
        let ok = std::process::Command::new("git")
            .args(["-C", dir.to_str().unwrap()])
            .args(args)
            .status()
            .unwrap()
            .success();
        assert!(ok, "git {args:?} failed");
    }

    /// A repo on branch `main` with one committed file and an uncommitted edit
    /// to it, so `compute_changed_files` reports exactly that file.
    fn repo_with_change(file: &str) -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path();
        git(p, &["init", "-q", "-b", "main"]);
        git(p, &["config", "user.email", "t@t"]);
        git(p, &["config", "user.name", "t"]);
        std::fs::write(p.join(file), "base\n").unwrap();
        git(p, &["add", "."]);
        git(p, &["commit", "-q", "-m", "init"]);
        std::fs::write(p.join(file), "changed\n").unwrap();
        dir
    }

    #[test]
    fn refresh_files_aggregates_across_repos() {
        let a = repo_with_change("a.txt");
        let b = repo_with_change("b.txt");

        let mut view = DiffView::test_default();
        view.repos = vec![
            DiffRepo {
                name: "A".to_string(),
                root: a.path().to_path_buf(),
            },
            DiffRepo {
                name: "B".to_string(),
                root: b.path().to_path_buf(),
            },
        ];
        view.base_branch = "main".to_string();
        view.refresh_files().unwrap();

        assert_eq!(view.files.len(), 2, "one changed file per repo");
        assert_eq!(view.file_repo_idx.len(), view.files.len());
        // Multi-repo: each file carries a repo name for the list prefix.
        assert!(view.file_repo_name(0).is_some());
        // Diff keys route to the file's own repo, so they are distinct and
        // rooted under the right worktree.
        let k0 = view.diff_key(0);
        let k1 = view.diff_key(1);
        assert_ne!(k0, k1);
        assert!(k0.starts_with(a.path()) || k0.starts_with(b.path()));
    }

    #[test]
    fn single_repo_has_no_repo_prefix() {
        let a = repo_with_change("only.txt");
        let mut view = DiffView::test_default();
        view.repos = vec![DiffRepo {
            name: "solo".to_string(),
            root: a.path().to_path_buf(),
        }];
        view.base_branch = "main".to_string();
        view.refresh_files().unwrap();

        assert_eq!(view.files.len(), 1);
        // A single-repo view has nothing to disambiguate, so no prefix.
        assert!(view.file_repo_name(0).is_none());
    }
}
