//! Syntax highlighting for the source view (M7).
//!
//! The whole document is highlighted once with [`syntect`] using its detected
//! language; the TUI caches the result and renders the per-line spans. We emit
//! *foreground* colours only — the terminal's own background is kept, and the
//! narrated region is marked by a gutter bar (in the TUI), not a background —
//! so the theme's own background never clashes with the user's terminal.
//!
//! Unknown or unsupported languages fall back to one default-styled span per
//! line, so the source view always renders (just without colour).

use ratatui::style::{Color, Modifier};
use syntect::easy::HighlightLines;
use syntect::highlighting::{FontStyle, ThemeSet};
use syntect::parsing::{SyntaxReference, SyntaxSet};

/// Tabs are expanded to this many columns so display width is stable (syntect
/// does not expand them, and a raw `\t` in a cell desyncs wrapping).
const TAB: &str = "    ";

/// One run of equally-styled text within a source line.
#[derive(Debug, Clone)]
pub struct Token {
    pub fg: Color,
    pub modifier: Modifier,
    pub text: String,
}

impl Token {
    fn plain(text: &str) -> Token {
        Token {
            fg: Color::Gray,
            modifier: Modifier::empty(),
            text: text.to_string(),
        }
    }
}

/// Highlight `source_lines` as `lang`, returning per-line styled spans. Unknown
/// or unsupported languages (and any highlighter error) fall back to one
/// default-styled span per line.
pub fn highlight_lines(source_lines: &[String], lang: &str) -> Vec<Vec<Token>> {
    let ps = SyntaxSet::load_defaults_newlines();
    let ts = ThemeSet::load_defaults();
    let (Some(syntax), Some(theme)) = (lang_syntax(&ps, lang), ts.themes.get("base16-ocean.dark"))
    else {
        return plain(source_lines);
    };

    let mut h = HighlightLines::new(syntax, theme);
    let mut out = Vec::with_capacity(source_lines.len());
    for line in source_lines {
        let expanded = line.replace('\t', TAB);
        // syntect wants the trailing newline to close the line's context.
        match h.highlight_line(&format!("{expanded}\n"), &ps) {
            Ok(ranges) => out.push(to_tokens(ranges)),
            Err(_) => out.push(line_plain(&expanded)),
        }
    }
    out
}

/// Map our language ids (see `lang_from_path` in `main.rs`) to a token syntect
/// recognises. syntect's default set covers the common languages; anything else
/// resolves to `None` and renders as plain text.
fn lang_syntax<'a>(ps: &'a SyntaxSet, lang: &str) -> Option<&'a SyntaxReference> {
    let token = match lang {
        "markdown" | "mdx" => "md",
        "rust" => "rs",
        "typescript" => "ts",
        "javascript" => "js",
        "python" => "py",
        "ruby" => "rb",
        "csharp" => "cs",
        "shell" => "sh",
        "restructuredtext" => "rst",
        "plaintext" | "" => return None,
        // go, c, cpp, java, php, sql, lua, yaml, toml, … match by extension.
        other => other,
    };
    ps.find_syntax_by_extension(token)
        .or_else(|| ps.find_syntax_by_token(token))
}

fn to_tokens(ranges: Vec<(syntect::highlighting::Style, &str)>) -> Vec<Token> {
    ranges
        .into_iter()
        .filter_map(|(style, text)| {
            let text = text.strip_suffix('\n').unwrap_or(text);
            if text.is_empty() {
                return None;
            }
            let fg = style.foreground;
            let mut modifier = Modifier::empty();
            if style.font_style.contains(FontStyle::BOLD) {
                modifier |= Modifier::BOLD;
            }
            if style.font_style.contains(FontStyle::ITALIC) {
                modifier |= Modifier::ITALIC;
            }
            if style.font_style.contains(FontStyle::UNDERLINE) {
                modifier |= Modifier::UNDERLINED;
            }
            Some(Token {
                fg: Color::Rgb(fg.r, fg.g, fg.b),
                modifier,
                text: text.to_string(),
            })
        })
        .collect()
}

/// Plain (uncoloured) fallback for the whole document.
fn plain(source_lines: &[String]) -> Vec<Vec<Token>> {
    source_lines
        .iter()
        .map(|l| line_plain(&l.replace('\t', TAB)))
        .collect()
}

/// Plain spans for one (already tab-expanded) line: empty for a blank line.
fn line_plain(expanded: &str) -> Vec<Token> {
    if expanded.is_empty() {
        Vec::new()
    } else {
        vec![Token::plain(expanded)]
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rust_is_tokenised_with_real_colours() {
        let lines = vec!["fn main() {}".to_string()];
        let hl = highlight_lines(&lines, "rust");
        assert_eq!(hl.len(), 1);
        // Keyword/identifier/punctuation split into several runs…
        assert!(hl[0].len() >= 2, "expected multiple tokens, got {:?}", hl[0]);
        // …and at least one carries a real syntax colour (not the gray fallback).
        assert!(
            hl[0].iter().any(|t| !matches!(t.fg, Color::Gray)),
            "expected a syntax colour: {:?}",
            hl[0]
        );
    }

    #[test]
    fn markdown_is_highlighted() {
        let lines = vec!["# Heading".to_string(), "plain text".to_string()];
        let hl = highlight_lines(&lines, "markdown");
        assert_eq!(hl.len(), 2);
        assert!(!hl[0].is_empty());
    }

    #[test]
    fn unknown_language_falls_back_to_plain() {
        let lines = vec!["some text here".to_string()];
        let hl = highlight_lines(&lines, "zzunknown");
        assert_eq!(hl.len(), 1);
        assert_eq!(hl[0].len(), 1, "plain fallback is a single span per line");
        assert_eq!(hl[0][0].text, "some text here");
        assert!(matches!(hl[0][0].fg, Color::Gray));
    }

    #[test]
    fn tabs_are_expanded() {
        let hl = highlight_lines(&vec!["\tx".to_string()], "zzunknown");
        assert!(hl[0][0].text.starts_with("    "), "tab expanded: {:?}", hl[0][0].text);
    }

    #[test]
    fn blank_line_yields_no_spans() {
        let hl = highlight_lines(&vec![String::new()], "rust");
        assert_eq!(hl.len(), 1);
        assert!(hl[0].is_empty());
    }
}
