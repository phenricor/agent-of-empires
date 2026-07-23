//! Commits view: the commits a session added on top of its base branch,
//! aggregated across every repo of a multi-repo workspace.

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::{
    layout::{Constraint, Direction, Layout, Rect},
    style::{Modifier, Style},
    text::{Line, Span},
    widgets::{
        Block, BorderType, Borders, Clear, Paragraph, Scrollbar, ScrollbarOrientation,
        ScrollbarState,
    },
    Frame,
};

use std::sync::Arc;

use crate::file_watch::FileWatchService;
use crate::git::diff::list_branches;
use crate::git::log::{commits_since_base, CommitInfo};
use crate::tui::diff::{BranchSelectState, DiffRepo};
use crate::tui::styles::Theme;

/// What the caller should do after a key press.
pub enum CommitsAction {
    Continue,
    Close,
}

/// A rendered row: either a repo section header or one commit under it.
enum Row {
    Repo { name: String, count: usize },
    Commit(CommitInfo),
}

pub struct CommitsView {
    /// Repos aggregated over; `repos[0]` is the primary used for branch listing.
    repos: Vec<DiffRepo>,
    rows: Vec<Row>,
    base_branch: String,
    /// Total commits across all repos, for the header summary.
    total_commits: usize,
    scroll_offset: u16,
    visible_lines: u16,

    /// Branch picker state, when open.
    branch_select: Option<BranchSelectState>,
    /// Session this view belongs to; `None` skips persisting a base override.
    session_id: Option<String>,
    profile: String,
    file_watch: Arc<FileWatchService>,
    /// Base override persisted to disk but not yet applied to HomeView's
    /// in-memory instance; drained via `take_pending_override` (mirrors the
    /// diff view, so HomeView's next commit doesn't clobber the new value).
    pending_override: Option<(String, Option<String>)>,
    error_message: Option<String>,
}

impl CommitsView {
    /// Collect the session's commits for every repo. Repos that error (e.g. the
    /// base ref does not exist there) are skipped rather than failing the view,
    /// matching how the diff view aggregates.
    pub fn new(
        repos: Vec<DiffRepo>,
        base_branch: String,
        session_id: Option<String>,
        profile: String,
        file_watch: Arc<FileWatchService>,
    ) -> Self {
        let mut view = Self {
            repos,
            rows: Vec::new(),
            base_branch,
            total_commits: 0,
            scroll_offset: 0,
            visible_lines: 20,
            branch_select: None,
            session_id,
            profile,
            file_watch,
            pending_override: None,
            error_message: None,
        };
        view.refresh();
        view
    }

    /// Rebuild the rows for the current base branch.
    fn refresh(&mut self) {
        let multi_repo = self.repos.len() > 1;
        let mut rows = Vec::new();
        let mut total_commits = 0;

        for repo in &self.repos {
            match commits_since_base(&repo.root, &self.base_branch) {
                Ok(commits) if commits.is_empty() => {}
                Ok(commits) => {
                    total_commits += commits.len();
                    // A single-repo session needs no section header.
                    if multi_repo {
                        rows.push(Row::Repo {
                            name: repo.name.clone(),
                            count: commits.len(),
                        });
                    }
                    rows.extend(commits.into_iter().map(Row::Commit));
                }
                Err(e) => {
                    tracing::debug!(target: "tui.commits", "skip repo {} in commits: {e}", repo.name);
                }
            }
        }

        self.rows = rows;
        self.total_commits = total_commits;
        self.scroll_offset = 0;
    }

    /// Open the base-branch picker, listing branches from the primary repo.
    fn open_branch_select(&mut self) {
        let Some(primary) = self.repos.first() else {
            return;
        };
        match list_branches(&primary.root) {
            Ok(branches) => {
                let selected = branches
                    .iter()
                    .position(|b| b == &self.base_branch)
                    .unwrap_or(0);
                self.branch_select = Some(BranchSelectState { branches, selected });
            }
            Err(e) => self.error_message = Some(format!("Failed to list branches: {e}")),
        }
    }

    /// Switch the base branch, persist it on the session, and rebuild. The
    /// override is the same field the diff view writes, so both views stay on
    /// the same base.
    fn select_branch(&mut self, branch: String) {
        self.base_branch = branch;
        self.branch_select = None;
        if let Err(e) = self.persist_base_override() {
            self.error_message = Some(format!("Failed to persist base branch: {e}"));
        }
        self.refresh();
    }

    fn persist_base_override(&mut self) -> anyhow::Result<()> {
        let Some(session_id) = self.session_id.clone() else {
            return Ok(());
        };
        let storage = crate::session::Storage::new(&self.profile, self.file_watch.clone())?;
        let new_override = Some(self.base_branch.clone());
        let id = session_id.clone();
        let value = new_override.clone();
        storage.update(|instances, _groups| {
            if let Some(inst) = instances.iter_mut().find(|i| i.id == id) {
                inst.base_branch_override = value.clone();
            }
            Ok(())
        })?;
        self.pending_override = Some((session_id, new_override));
        Ok(())
    }

    /// Drain a persisted base override so HomeView can sync its in-memory copy.
    pub fn take_pending_override(&mut self) -> Option<(String, Option<String>)> {
        self.pending_override.take()
    }

    pub fn handle_key(&mut self, key: KeyEvent) -> CommitsAction {
        // The branch picker owns input while it is open.
        if let Some(state) = &mut self.branch_select {
            match key.code {
                KeyCode::Esc | KeyCode::Char('q') | KeyCode::Char('Q') => self.branch_select = None,
                KeyCode::Up | KeyCode::Char('k') if state.selected > 0 => state.selected -= 1,
                KeyCode::Down | KeyCode::Char('j') if state.selected + 1 < state.branches.len() => {
                    state.selected += 1
                }
                KeyCode::Enter => {
                    let chosen = state.branches.get(state.selected).cloned();
                    if let Some(branch) = chosen {
                        self.select_branch(branch);
                    }
                }
                _ => {}
            }
            return CommitsAction::Continue;
        }

        self.error_message = None;
        match (key.code, key.modifiers) {
            (KeyCode::Esc, _) | (KeyCode::Char('q'), _) | (KeyCode::Char('Q'), _) => {
                CommitsAction::Close
            }
            (KeyCode::Char('b'), _) => {
                self.open_branch_select();
                CommitsAction::Continue
            }
            (KeyCode::Down, _) | (KeyCode::Char('j'), _) => {
                self.scroll_by(1);
                CommitsAction::Continue
            }
            (KeyCode::Up, _) | (KeyCode::Char('k'), _) => {
                self.scroll_offset = self.scroll_offset.saturating_sub(1);
                CommitsAction::Continue
            }
            (KeyCode::Char('d'), KeyModifiers::CONTROL) | (KeyCode::PageDown, _) => {
                self.scroll_by(self.visible_lines / 2);
                CommitsAction::Continue
            }
            (KeyCode::Char('u'), KeyModifiers::CONTROL) | (KeyCode::PageUp, _) => {
                self.scroll_offset = self.scroll_offset.saturating_sub(self.visible_lines / 2);
                CommitsAction::Continue
            }
            (KeyCode::Char('g'), _) | (KeyCode::Home, _) => {
                self.scroll_offset = 0;
                CommitsAction::Continue
            }
            (KeyCode::Char('G'), _) | (KeyCode::End, _) => {
                self.scroll_offset = self.max_scroll();
                CommitsAction::Continue
            }
            _ => CommitsAction::Continue,
        }
    }

    fn max_scroll(&self) -> u16 {
        (self.rows.len() as u16).saturating_sub(self.visible_lines)
    }

    fn scroll_by(&mut self, amount: u16) {
        self.scroll_offset = (self.scroll_offset + amount).min(self.max_scroll());
    }

    pub fn render(&mut self, frame: &mut Frame, area: Rect, theme: &Theme) {
        frame.render_widget(Clear, area);

        let layout = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Length(3),
                Constraint::Min(5),
                Constraint::Length(3),
            ])
            .split(area);

        self.render_header(frame, layout[0], theme);
        self.render_list(frame, layout[1], theme);
        self.render_footer(frame, layout[2], theme);

        if self.branch_select.is_some() {
            self.render_branch_select(frame, area, theme);
        }
    }

    fn render_branch_select(&self, frame: &mut Frame, area: Rect, theme: &Theme) {
        let Some(state) = &self.branch_select else {
            return;
        };

        let width = 50u16.min(area.width);
        let height = (state.branches.len() as u16 + 2).min(area.height).max(3);
        let dialog = Rect {
            x: area.x + (area.width.saturating_sub(width)) / 2,
            y: area.y + (area.height.saturating_sub(height)) / 2,
            width,
            height,
        };
        frame.render_widget(Clear, dialog);

        let block = Block::default()
            .title(" Base branch ")
            .borders(Borders::ALL)
            .border_type(BorderType::Rounded)
            .border_style(Style::default().fg(theme.accent))
            .style(Style::default().bg(theme.background));
        let inner = block.inner(dialog);
        frame.render_widget(block, dialog);

        // Keep the selection visible when the branch list is taller than the box.
        let visible = inner.height as usize;
        let start = state.selected.saturating_sub(visible.saturating_sub(1));
        let lines: Vec<Line> = state
            .branches
            .iter()
            .enumerate()
            .skip(start)
            .take(visible)
            .map(|(i, b)| {
                let selected = i == state.selected;
                Line::from(Span::styled(
                    format!("{} {b}", if selected { ">" } else { " " }),
                    if selected {
                        Style::default()
                            .fg(theme.accent)
                            .add_modifier(Modifier::BOLD)
                    } else {
                        Style::default().fg(theme.text)
                    },
                ))
            })
            .collect();
        frame.render_widget(Paragraph::new(lines), inner);
    }

    fn render_header(&self, frame: &mut Frame, area: Rect, theme: &Theme) {
        let block = Block::default()
            .borders(Borders::BOTTOM)
            .border_style(Style::default().fg(theme.border));
        let inner = block.inner(area);
        frame.render_widget(block, area);

        let header = Line::from(vec![
            Span::styled(
                "  Commits ",
                Style::default().fg(theme.text).add_modifier(Modifier::BOLD),
            ),
            Span::styled("since ", Style::default().fg(theme.dimmed)),
            Span::styled(&self.base_branch, Style::default().fg(theme.accent)),
            Span::styled("  |  ", Style::default().fg(theme.border)),
            Span::styled(
                format!("{} commit(s)", self.total_commits),
                Style::default().fg(theme.dimmed),
            ),
        ]);
        frame.render_widget(Paragraph::new(header), inner);
    }

    fn render_list(&mut self, frame: &mut Frame, area: Rect, theme: &Theme) {
        let block = Block::default()
            .borders(Borders::ALL)
            .border_type(BorderType::Rounded)
            .border_style(Style::default().fg(theme.accent));
        let inner = block.inner(area);
        frame.render_widget(block, area);

        self.visible_lines = inner.height;

        if self.rows.is_empty() {
            let msg = Paragraph::new("No commits on this branch yet.")
                .style(Style::default().fg(theme.dimmed));
            frame.render_widget(msg, inner);
            return;
        }

        // Clamp after `visible_lines` is known, so a resize cannot strand the
        // view past the end of the list.
        self.scroll_offset = self.scroll_offset.min(self.max_scroll());
        let start = self.scroll_offset as usize;

        let lines: Vec<Line> = self
            .rows
            .iter()
            .skip(start)
            .take(inner.height as usize)
            .map(|row| match row {
                Row::Repo { name, count } => Line::from(vec![
                    Span::styled(
                        format!("{name} "),
                        Style::default()
                            .fg(theme.diff_header)
                            .add_modifier(Modifier::BOLD),
                    ),
                    Span::styled(format!("({count})"), Style::default().fg(theme.dimmed)),
                ]),
                Row::Commit(c) => Line::from(vec![
                    Span::styled(
                        format!("  {} ", c.short_hash),
                        Style::default().fg(theme.accent),
                    ),
                    Span::styled(c.subject.clone(), Style::default().fg(theme.text)),
                    Span::styled(
                        format!("  {} · {}", c.author, c.date),
                        Style::default().fg(theme.dimmed),
                    ),
                ]),
            })
            .collect();

        frame.render_widget(Paragraph::new(lines), inner);

        if self.rows.len() > inner.height as usize {
            let scrollbar_area = Rect {
                x: area.x + area.width - 1,
                y: area.y + 1,
                width: 1,
                height: area.height.saturating_sub(2),
            };
            let mut state = ScrollbarState::new(self.max_scroll() as usize + 1).position(start);
            frame.render_stateful_widget(
                Scrollbar::new(ScrollbarOrientation::VerticalRight)
                    .begin_symbol(Some("↑"))
                    .end_symbol(Some("↓")),
                scrollbar_area,
                &mut state,
            );
        }
    }

    fn render_footer(&self, frame: &mut Frame, area: Rect, theme: &Theme) {
        let block = Block::default()
            .borders(Borders::TOP)
            .border_style(Style::default().fg(theme.border));
        let inner = block.inner(area);
        frame.render_widget(block, area);

        if let Some(err) = &self.error_message {
            frame.render_widget(
                Paragraph::new(Line::from(Span::styled(
                    format!("  {err}"),
                    Style::default().fg(theme.error),
                ))),
                inner,
            );
            return;
        }

        let hints = Line::from(vec![
            Span::styled("  j/k", Style::default().fg(theme.accent)),
            Span::styled(": scroll  ", Style::default().fg(theme.dimmed)),
            Span::styled("b", Style::default().fg(theme.accent)),
            Span::styled(": base branch  ", Style::default().fg(theme.dimmed)),
            Span::styled("Ctrl+u/d", Style::default().fg(theme.accent)),
            Span::styled(": half page  ", Style::default().fg(theme.dimmed)),
            Span::styled("g/G", Style::default().fg(theme.accent)),
            Span::styled(": top/bottom  ", Style::default().fg(theme.dimmed)),
            Span::styled("q/Esc", Style::default().fg(theme.accent)),
            Span::styled(": close", Style::default().fg(theme.dimmed)),
        ]);
        frame.render_widget(Paragraph::new(hints), inner);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::{Path, PathBuf};

    fn git(dir: &Path, args: &[&str]) {
        let ok = std::process::Command::new("git")
            .args(["-C", dir.to_str().unwrap()])
            .args(args)
            .status()
            .unwrap()
            .success();
        assert!(ok, "git {args:?} failed");
    }

    fn repo_with_commits(subjects: &[&str]) -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path();
        git(p, &["init", "-q", "-b", "main"]);
        git(p, &["config", "user.email", "t@t"]);
        git(p, &["config", "user.name", "Tester"]);
        std::fs::write(p.join("base.txt"), "base\n").unwrap();
        git(p, &["add", "."]);
        git(p, &["commit", "-q", "-m", "base"]);
        git(p, &["checkout", "-q", "-b", "session"]);
        for (i, s) in subjects.iter().enumerate() {
            std::fs::write(p.join(format!("f{i}.txt")), "x\n").unwrap();
            git(p, &["add", "."]);
            git(p, &["commit", "-q", "-m", s]);
        }
        dir
    }

    fn repo(name: &str, root: PathBuf) -> DiffRepo {
        DiffRepo {
            name: name.to_string(),
            root,
        }
    }

    #[test]
    fn aggregates_commits_across_repos_with_headers() {
        let a = repo_with_commits(&["a one", "a two"]);
        let b = repo_with_commits(&["b one"]);

        let view = CommitsView::new(
            vec![
                repo("A", a.path().to_path_buf()),
                repo("B", b.path().to_path_buf()),
            ],
            "main".to_string(),
            None,
            String::new(),
            FileWatchService::noop(),
        );

        assert_eq!(view.total_commits, 3);
        // 2 repo headers + 3 commits.
        assert_eq!(view.rows.len(), 5);
        assert!(matches!(view.rows[0], Row::Repo { .. }));
    }

    #[test]
    fn single_repo_omits_the_section_header() {
        let a = repo_with_commits(&["only one"]);
        let view = CommitsView::new(
            vec![repo("A", a.path().to_path_buf())],
            "main".to_string(),
            None,
            String::new(),
            FileWatchService::noop(),
        );

        assert_eq!(view.total_commits, 1);
        assert_eq!(view.rows.len(), 1, "no header for a single repo");
        assert!(matches!(view.rows[0], Row::Commit(_)));
    }

    #[test]
    fn no_commits_yields_an_empty_view() {
        let a = repo_with_commits(&[]);
        let view = CommitsView::new(
            vec![repo("A", a.path().to_path_buf())],
            "main".to_string(),
            None,
            String::new(),
            FileWatchService::noop(),
        );
        assert_eq!(view.total_commits, 0);
        assert!(view.rows.is_empty());
    }

    #[test]
    fn q_closes_and_j_scrolls() {
        let a = repo_with_commits(&["one", "two", "three"]);
        let mut view = CommitsView::new(
            vec![repo("A", a.path().to_path_buf())],
            "main".to_string(),
            None,
            String::new(),
            FileWatchService::noop(),
        );
        view.visible_lines = 1;

        let k = |c: char| KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE);
        assert!(matches!(view.handle_key(k('j')), CommitsAction::Continue));
        assert_eq!(view.scroll_offset, 1);
        assert!(matches!(view.handle_key(k('G')), CommitsAction::Continue));
        assert_eq!(view.scroll_offset, view.max_scroll());
        assert!(matches!(view.handle_key(k('q')), CommitsAction::Close));
        // Uppercase Q closes too, matching the diff view.
        assert!(matches!(view.handle_key(k('Q')), CommitsAction::Close));
    }

    #[test]
    fn b_opens_the_branch_picker_and_switching_base_rebuilds() {
        let a = repo_with_commits(&["one", "two"]);
        let mut view = CommitsView::new(
            vec![repo("A", a.path().to_path_buf())],
            "main".to_string(),
            None,
            String::new(),
            FileWatchService::noop(),
        );
        assert_eq!(view.total_commits, 2);

        let k = |c: char| KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE);
        view.handle_key(k('b'));
        assert!(view.branch_select.is_some(), "b should open the picker");

        view.handle_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
        assert!(
            view.branch_select.is_none(),
            "Esc should dismiss the picker"
        );

        // Comparing the session branch against itself leaves no commits, which
        // proves the switch rebuilt the rows.
        view.select_branch("session".to_string());
        assert_eq!(view.base_branch, "session");
        assert_eq!(view.total_commits, 0);
        assert!(view.rows.is_empty());
    }
}
