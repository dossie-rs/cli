//! Server-side syntax highlighting for fenced code blocks.
//!
//! Highlighting is class-based: [`highlight`] turns source into HTML `<span>`s
//! carrying `hl-`-prefixed scope classes, and [`highlight_css`] produces the
//! matching stylesheet with a light and a dark theme scoped to the site's
//! `data-theme` attribute. That split lets the theme toggle switch highlighting
//! colors with no re-render, and degrades to plain (uncolored) code when CSS or
//! JS is unavailable.
//!
//! The `SyntaxSet` and generated CSS are built once, lazily. Diagram languages
//! (mermaid, svgbob) are deliberately *not* highlightable — they are rendered
//! separately (svgbob to SVG, mermaid client-side), so their code blocks must
//! not pass through here (see `render_markdown`).

use lazy_static::lazy_static;
use regex::Regex;
use syntect::highlighting::ThemeSet;
use syntect::html::{css_for_theme_with_class_style, ClassStyle, ClassedHTMLGenerator};
use syntect::parsing::SyntaxSet;
use syntect::util::LinesWithEndings;

/// Prefix on every generated class, so highlight styles can never collide with
/// the site's own CSS. Must be `'static` (a syntect API requirement).
const CLASS_STYLE: ClassStyle = ClassStyle::SpacedPrefixed { prefix: "hl-" };

/// Default theme keys from syntect's bundled set: a dark and a light variant.
const DARK_THEME: &str = "base16-ocean.dark";
const LIGHT_THEME: &str = "InspiredGitHub";

lazy_static! {
    static ref SYNTAX_SET: SyntaxSet = SyntaxSet::load_defaults_newlines();
    static ref HIGHLIGHT_CSS: String = build_css();
}

/// Language tokens rendered separately from raw source; never highlighted.
fn is_diagram_lang(token: &str) -> bool {
    matches!(token.to_ascii_lowercase().as_str(), "mermaid" | "svgbob")
}

/// Whether a fence's language token can be highlighted: non-empty, not a
/// diagram language, and backed by a known syntax.
pub fn is_highlightable(token: &str) -> bool {
    !token.is_empty() && !is_diagram_lang(token) && SYNTAX_SET.find_syntax_by_token(token).is_some()
}

/// Render `code` as class-annotated `<span>` HTML for the given language token.
///
/// Returns `None` when the token has no known syntax; callers should then fall
/// back to a plain escaped `<pre><code>` block. The returned string is the inner
/// content only — callers wrap it in `<pre><code>`.
pub fn highlight(token: &str, code: &str) -> Option<String> {
    let syntax = SYNTAX_SET.find_syntax_by_token(token)?;
    let mut generator =
        ClassedHTMLGenerator::new_with_class_style(syntax, &SYNTAX_SET, CLASS_STYLE);
    for line in LinesWithEndings::from(code) {
        generator
            .parse_html_for_line_which_includes_newline(line)
            .ok()?;
    }
    Some(generator.finalize())
}

/// The generated highlight stylesheet, inlined into pages that contain a
/// highlighted code block.
pub fn highlight_css() -> &'static str {
    &HIGHLIGHT_CSS
}

/// Build the combined light + dark stylesheet, each theme scoped to the matching
/// `data-theme` value so the site's theme toggle switches colors without a
/// re-render. Missing themes are skipped rather than panicking.
fn build_css() -> String {
    let themes = ThemeSet::load_defaults();
    let mut css = String::new();
    for (key, scope) in [
        (DARK_THEME, ":root[data-theme='dark'] .doc-content"),
        (LIGHT_THEME, ":root[data-theme='light'] .doc-content"),
    ] {
        if let Some(theme) = themes.themes.get(key) {
            if let Ok(rules) = css_for_theme_with_class_style(theme, CLASS_STYLE) {
                css.push_str(&scope_css(&rules, scope));
            }
        }
    }
    css
}

/// Prefix every selector in a flat syntect stylesheet with `scope`.
///
/// syntect emits only flat rules (no nested braces) preceded by a single
/// comment, so splitting on `}` yields one `selectors { decls` chunk per rule.
/// Each comma-separated selector is prefixed so the two themes coexist and
/// switch on `data-theme`.
fn scope_css(sheet: &str, scope: &str) -> String {
    lazy_static! {
        static ref COMMENT: Regex = Regex::new(r"(?s)/\*.*?\*/").unwrap();
    }
    let stripped = COMMENT.replace_all(sheet, "");
    let mut out = String::new();
    for rule in stripped.split('}') {
        let rule = rule.trim();
        if rule.is_empty() {
            continue;
        }
        let Some(brace) = rule.find('{') else {
            continue;
        };
        let (selectors, body) = rule.split_at(brace);
        let body = &body[1..]; // drop the leading '{'
        let scoped = selectors
            .split(',')
            .map(|s| format!("{scope} {}", s.trim()))
            .collect::<Vec<_>>()
            .join(", ");
        out.push_str(&scoped);
        out.push_str(" {\n");
        out.push_str(body.trim());
        out.push_str("\n}\n");
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn known_language_is_highlightable_by_name_and_extension() {
        assert!(is_highlightable("rust"));
        assert!(is_highlightable("rs"));
        assert!(is_highlightable("Python"));
    }

    #[test]
    fn diagram_languages_and_unknowns_are_not_highlightable() {
        assert!(!is_highlightable("mermaid"));
        assert!(!is_highlightable("svgbob"));
        assert!(!is_highlightable(""));
        assert!(!is_highlightable("definitely-not-a-language"));
    }

    #[test]
    fn highlight_emits_prefixed_scope_spans() {
        let html = highlight("rust", "fn main() {}\n").expect("rust highlights");
        assert!(html.contains("class=\"hl-"), "no prefixed spans: {html}");
        // Source text survives, HTML-escaped by the generator.
        assert!(html.contains("main"), "source dropped: {html}");
    }

    #[test]
    fn highlight_escapes_html_in_source() {
        let html = highlight("html", "<b>&\"</b>\n").expect("html highlights");
        assert!(!html.contains("<b>"), "raw tag leaked: {html}");
        assert!(
            html.contains("&lt;") && html.contains("&amp;"),
            "not escaped: {html}"
        );
    }

    #[test]
    fn unknown_language_returns_none() {
        assert!(highlight("", "plain\n").is_none());
        assert!(highlight("definitely-not-a-language", "plain\n").is_none());
    }

    #[test]
    fn css_scopes_both_themes_and_is_valid() {
        let css = highlight_css();
        assert!(css.contains(":root[data-theme='dark'] .doc-content .hl-"));
        assert!(css.contains(":root[data-theme='light'] .doc-content .hl-"));
        // No stray comment survived scoping, and braces are balanced.
        assert!(!css.contains("/*"), "comment leaked into scoped css");
        assert_eq!(
            css.matches('{').count(),
            css.matches('}').count(),
            "unbalanced braces in generated css"
        );
    }
}
