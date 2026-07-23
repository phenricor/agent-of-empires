//! Syntax highlighting for the TUI diff view.
//!
//! Diff lines are highlighted independently (the diff only carries changed
//! lines, not whole-file context), so multi-line constructs like block comments
//! may color imperfectly; this matches how most terminal diff viewers behave
//! and keeps highlighting stateless and cheap. The add/delete signal stays on
//! the `+`/`-` marker; only the code content is syntax-colored.

use std::path::Path;
use std::sync::OnceLock;

use ratatui::style::{Color, Style};
use ratatui::text::Span;
use syntect::easy::HighlightLines;
use syntect::highlighting::{FontStyle, Style as SynStyle, Theme, ThemeSet};
use syntect::parsing::SyntaxSet;
use syntect::util::LinesWithEndings;

struct Highlighter {
    syntaxes: SyntaxSet,
    theme: Theme,
}

fn highlighter() -> &'static Highlighter {
    static H: OnceLock<Highlighter> = OnceLock::new();
    H.get_or_init(|| {
        // two-face bundles bat's extended syntax set (TypeScript, Razor, etc.),
        // covering far more than syntect's small default set.
        let syntaxes = two_face::syntax::extra_newlines();
        let mut themes = ThemeSet::load_defaults();
        // A dark theme that reads well on the app's dark UI. Fall back to any
        // available theme if the expected key is ever missing.
        let theme = themes
            .themes
            .remove("base16-ocean.dark")
            .or_else(|| themes.themes.values().next().cloned())
            .expect("syntect ships at least one default theme");
        Highlighter { syntaxes, theme }
    })
}

/// Map an extension with no dedicated syntax to a close relative's extension.
fn alias_extension(ext: &str) -> Option<&'static str> {
    match ext.to_ascii_lowercase().as_str() {
        "cshtml" | "razor" | "vbhtml" => Some("html"),
        "csproj" | "vbproj" | "props" | "targets" | "config" | "nuspec" | "resx" => Some("xml"),
        _ => None,
    }
}

/// Map a syntect color to a ratatui truecolor. Alpha is ignored.
fn to_rgb(c: syntect::highlighting::Color) -> Color {
    Color::Rgb(c.r, c.g, c.b)
}

fn to_style(s: SynStyle) -> Style {
    let mut style = Style::default().fg(to_rgb(s.foreground));
    if s.font_style.contains(FontStyle::BOLD) {
        style = style.add_modifier(ratatui::style::Modifier::BOLD);
    }
    if s.font_style.contains(FontStyle::ITALIC) {
        style = style.add_modifier(ratatui::style::Modifier::ITALIC);
    }
    style
}

/// Highlight a single line of code for the file at `path`, returning styled
/// spans (owned, so they outlive the borrowed input). Returns `None` when no
/// syntax matches the file, letting the caller fall back to a flat style.
pub fn highlight_line(path: &Path, content: &str) -> Option<Vec<Span<'static>>> {
    let h = highlighter();
    let ext = path.extension().and_then(|e| e.to_str());
    let syntax = ext
        .and_then(|ext| h.syntaxes.find_syntax_by_extension(ext))
        .or_else(|| {
            path.file_name()
                .and_then(|n| n.to_str())
                .and_then(|name| h.syntaxes.find_syntax_by_extension(name))
        })
        .or_else(|| {
            // Extensions with no dedicated syntax that are really another
            // language (Razor is HTML-ish, project files are XML).
            ext.and_then(alias_extension)
                .and_then(|a| h.syntaxes.find_syntax_by_extension(a))
        })?;

    let mut hl = HighlightLines::new(syntax, &h.theme);
    let mut spans: Vec<Span<'static>> = Vec::new();
    // LinesWithEndings yields the whole `content` as one line here; iterating
    // keeps us correct if a trailing newline slips through.
    for line in LinesWithEndings::from(content) {
        let ranges = hl.highlight_line(line, &h.syntaxes).ok()?;
        for (style, text) in ranges {
            let text = text.trim_end_matches('\n');
            if text.is_empty() {
                continue;
            }
            spans.push(Span::styled(text.to_string(), to_style(style)));
        }
    }
    Some(spans)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn highlights_rust_into_multiple_spans() {
        let spans =
            highlight_line(Path::new("x.rs"), "let x = 1;").expect("rust syntax should resolve");
        // A keyword + identifier + punctuation should not collapse to one span.
        assert!(spans.len() > 1, "expected multiple tokens, got {:?}", spans);
        let joined: String = spans.iter().map(|s| s.content.as_ref()).collect();
        assert_eq!(joined, "let x = 1;");
    }

    #[test]
    fn unknown_extension_returns_none() {
        assert!(highlight_line(Path::new("data.zzz_unknown"), "plain text").is_none());
    }

    #[test]
    fn covers_extended_and_aliased_languages() {
        // two-face adds TypeScript; the alias table routes Razor/csproj.
        for (file, code) in [
            ("app.ts", "const x: number = 1;"),
            ("Foo.cs", "public class Foo {}"),
            ("View.cshtml", "<div>@Model.Name</div>"),
            (
                "App.csproj",
                "<Project Sdk=\"Microsoft.NET.Sdk\"></Project>",
            ),
        ] {
            assert!(
                highlight_line(Path::new(file), code).is_some(),
                "{file} should syntax-highlight"
            );
        }
    }
}
