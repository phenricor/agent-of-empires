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

use crate::git::log::{commits_since_base, CommitInfo};
use crate::tui::diff::DiffRepo;
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
    rows: Vec<Row>,
    base_branch: String,
    /// Total commits across all repos, for the header summary.
    total_commits: usize,
    scroll_offset: u16,
    visible_lines: u16,
}

impl CommitsView {
    /// Collect the session's commits for every repo. Repos that error (e.g. the
    /// base ref does not exist there) are skipped rather than failing the view,
    /// matching how the diff view aggregates.
    pub fn new(repos: &[DiffRepo], base_branch: String) -> Self {
        let multi_repo = repos.len() > 1;
        let mut rows = Vec::new();
        let mut total_commits = 0;

        for repo in repos {
            match commits_since_base(&repo.root, &base_branch) {
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

        Self {
            rows,
            base_branch,
            total_commits,
            scroll_offset: 0,
            visible_lines: 20,
        }
    }

    pub fn handle_key(&mut self, key: KeyEvent) -> CommitsAction {
        match (key.code, key.modifiers) {
            (KeyCode::Esc, _) | (KeyCode::Char('q'), _) => CommitsAction::Close,
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

        let hints = Line::from(vec![
            Span::styled("  j/k", Style::default().fg(theme.accent)),
            Span::styled(": scroll  ", Style::default().fg(theme.dimmed)),
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
            &[
                repo("A", a.path().to_path_buf()),
                repo("B", b.path().to_path_buf()),
            ],
            "main".to_string(),
        );

        assert_eq!(view.total_commits, 3);
        // 2 repo headers + 3 commits.
        assert_eq!(view.rows.len(), 5);
        assert!(matches!(view.rows[0], Row::Repo { .. }));
    }

    #[test]
    fn single_repo_omits_the_section_header() {
        let a = repo_with_commits(&["only one"]);
        let view = CommitsView::new(&[repo("A", a.path().to_path_buf())], "main".to_string());

        assert_eq!(view.total_commits, 1);
        assert_eq!(view.rows.len(), 1, "no header for a single repo");
        assert!(matches!(view.rows[0], Row::Commit(_)));
    }

    #[test]
    fn no_commits_yields_an_empty_view() {
        let a = repo_with_commits(&[]);
        let view = CommitsView::new(&[repo("A", a.path().to_path_buf())], "main".to_string());
        assert_eq!(view.total_commits, 0);
        assert!(view.rows.is_empty());
    }

    #[test]
    fn q_closes_and_j_scrolls() {
        let a = repo_with_commits(&["one", "two", "three"]);
        let mut view = CommitsView::new(&[repo("A", a.path().to_path_buf())], "main".to_string());
        view.visible_lines = 1;

        let k = |c: char| KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE);
        assert!(matches!(view.handle_key(k('j')), CommitsAction::Continue));
        assert_eq!(view.scroll_offset, 1);
        assert!(matches!(view.handle_key(k('G')), CommitsAction::Continue));
        assert_eq!(view.scroll_offset, view.max_scroll());
        assert!(matches!(view.handle_key(k('q')), CommitsAction::Close));
    }
}
