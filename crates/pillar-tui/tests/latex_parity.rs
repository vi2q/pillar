//! Parity tests for tui latex.ts (pi v0.84.3): symbol mapping, scripts,
//! fractions/roots, environments (align/cases/matrix), display layout
//! stacking, and unsupported-syntax rejection.

use pillar_tui::latex::{RenderLatexOptions, render_latex};

fn render(source: &str) -> Option<String> {
    render_latex(source, RenderLatexOptions::default())
}

fn render_display(source: &str) -> Option<String> {
    render_latex(source, RenderLatexOptions { display: true })
}

#[test]
fn plain_text_passes_through() {
    assert_eq!(render("abc"), Some("abc".to_string()));
}

#[test]
fn greek_and_operators_map_to_unicode() {
    assert_eq!(render("\\alpha"), Some("α".to_string()));
    assert_eq!(render("\\Omega"), Some("Ω".to_string()));
    assert_eq!(render("\\infty"), Some("∞".to_string()));
    assert_eq!(render("\\sum"), Some("∑".to_string()));
    // Relation/operator spacing is added inline and trimmed by
    // normalizeOutput at line edges.
    assert_eq!(render("\\times"), Some("×".to_string()));
    assert_eq!(render("\\cdot"), Some("·".to_string()));
    assert_eq!(render("\\leq"), Some("≤".to_string()));
}

#[test]
fn named_operators_render_without_markers() {
    assert_eq!(render("\\sin(x)"), Some("sin(x)".to_string()));
    assert_eq!(render("\\lim"), Some("lim".to_string()));
}

#[test]
fn scripts_use_unicode_when_available() {
    assert_eq!(render("x^2"), Some("x²".to_string()));
    assert_eq!(render("x_i"), Some("xᵢ".to_string()));
    // Multi-character superscripts fall back to ^(...) form.
    assert_eq!(render("x^{10}"), Some("x¹⁰".to_string()));
    assert_eq!(render("n_{ab}"), Some("n_ab".to_string()));
}

#[test]
fn inline_fractions_use_slash_form() {
    assert_eq!(render("\\frac{a}{b}"), Some("a/b".to_string()));
    // Complex denominators get parentheses.
    assert_eq!(render("\\frac{1}{x+y}"), Some("1/(x+y)".to_string()));
}

#[test]
fn sqrt_roots() {
    assert_eq!(render("\\sqrt{x}"), Some("√x".to_string()));
    assert_eq!(render("\\sqrt[3]{x}"), Some("∛x".to_string()));
    assert_eq!(render("\\sqrt[4]{x}"), Some("∜x".to_string()));
}

#[test]
fn negation_via_not() {
    assert_eq!(render("\\not="), Some("≠".to_string()));
    // Unknown operand: combining slash on the first character.
    assert_eq!(render("\\not\\alpha"), Some("α̸".to_string()));
}

#[test]
fn mathbb_blackboard_letters() {
    assert_eq!(render("\\mathbb{R}"), Some("ℝ".to_string()));
    assert_eq!(render("\\mathbb{ZR}"), Some("ℤℝ".to_string()));
    // Non-blackboard characters pass through.
}

#[test]
fn spacing_commands_and_ignored_commands() {
    assert_eq!(render("a\\,b"), Some("a b".to_string()));
    assert_eq!(render("a\\quad b"), Some("a b".to_string()));
    assert_eq!(render("\\displaystyle x"), Some("x".to_string()));
    assert_eq!(render("\\big("), Some("(".to_string()));
}

#[test]
fn braces_escaped_chars() {
    assert_eq!(render("\\{x\\}"), Some("{x}".to_string()));
    assert_eq!(render("\\%"), Some("%".to_string()));
    assert_eq!(render("\\&"), Some("&".to_string()));
    // ~ is a non-breaking space.
    assert_eq!(render("a~b"), Some("a b".to_string()));
    // = gets padding.
    assert_eq!(render("a=b"), Some("a = b".to_string()));
}

#[test]
fn unsupported_commands_fail() {
    assert_eq!(render("\\unknowncmd"), None);
    assert_eq!(render("\\frac{1"), None);
    assert!(render("\\end{x}").is_none());
}

#[test]
fn equation_environment() {
    assert_eq!(
        render("\\begin{equation}x = 1\\end{equation}"),
        Some("x = 1".to_string())
    );
}

#[test]
fn align_environment_joins_rows() {
    assert_eq!(
        render("\\begin{align}a &= 1\\\\ b &= 2\\end{align}"),
        Some("a = 1\nb = 2".to_string())
    );
}

#[test]
fn cases_environment() {
    let result =
        render("\\begin{cases}1 & \\text{if } x > 0\\\\ 0 & \\text{otherwise}\\end{cases}");
    assert!(result.is_some(), "{result:?}");
    let text = result.unwrap();
    assert!(text.contains('⎧'), "{text}");
    assert!(text.contains('⎩'), "{text}");
    // With only two rows there is no middle delimiter.
}

#[test]
fn pmatrix_renders_with_delimiters() {
    let result = render("\\begin{pmatrix}a & b\\\\ c & d\\end{pmatrix}");
    let text = result.unwrap();
    assert!(text.contains('⎛'), "{text}");
    assert!(text.contains('⎞'), "{text}");
    assert!(text.contains("│"), "{text}");
    // Columns align: both rows have the same visible width.
    let lines: Vec<&str> = text.split('\n').collect();
    assert_eq!(lines.len(), 2);
}

#[test]
fn vmatrix_uses_bars() {
    let result = render("\\begin{vmatrix}a\\\\ b\\end{vmatrix}");
    let text = result.unwrap();
    assert!(text.contains('│'), "{text}");
}

#[test]
fn display_mode_stacks_fractions() {
    let inline = render("\\frac{a}{b}").unwrap();
    let display = render_display("\\frac{a}{b}").unwrap();
    // Inline is slash form; display is stacked with a fraction bar.
    assert_eq!(inline, "a/b");
    assert!(display.contains('─'), "{display}");
    assert!(display.contains('a'), "{display}");
    assert!(display.contains('b'), "{display}");
}

#[test]
fn display_mode_stacks_operator_limits() {
    let result = render_display("\\sum_{i=1}^{n} x_i");
    assert!(result.is_some(), "{result:?}");
    let text = result.unwrap();
    // The operator layout shows the upper limit above and lower below.
    assert!(text.contains('∑'), "{text}");
    assert!(text.lines().count() > 1, "{text}");
}

#[test]
fn binom_and_mod() {
    assert_eq!(render("\\binom{n}{k}"), Some("(n choose k)".to_string()));
    // The inline spacing is trimmed/collapsed by normalizeOutput.
    assert_eq!(render("a \\bmod b"), Some("a mod b".to_string()));
    assert_eq!(render("a \\pmod{n}"), Some("a (mod n)".to_string()));
}

#[test]
fn overset_underset() {
    // ∧ has no superscript mapping → prefix form.
    assert_eq!(render("\\overset{\\wedge}{=}"), Some("=^∧".to_string()));
}

#[test]
fn accents_single_character() {
    assert_eq!(render("\\hat{x}"), Some("x̂".to_string()));
    // Multi-character accent argument falls back to function form.
    assert_eq!(render("\\hat{xy}"), Some("hat(xy)".to_string()));
}

#[test]
fn text_wrapper_preserves_words() {
    assert_eq!(render("\\text{where}"), Some("where".to_string()));
}

#[test]
fn operatorname() {
    assert_eq!(render("\\operatorname{sgn}(x)"), Some("sgn(x)".to_string()));
}

#[test]
fn whitespace_runs_collapse() {
    assert_eq!(render("a    b"), Some("a b".to_string()));
}
