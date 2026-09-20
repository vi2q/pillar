//! Port of packages/tui/src/latex.ts (pi v0.84.3): LaTeX math rendered to
//! terminal-friendly Unicode text — the full symbol table, named
//! operators, scripts, fractions/roots, accents, environments (align /
//! cases / matrices), and the stacked layout renderer.
//!
//! divergence: upstream regexes (Unicode property classes, lookbehind)
//! become hand-written character classification; regex `\s` semantics map
//! to `char::is_whitespace`.

use std::collections::BTreeMap;

fn visible_width(text: &str) -> usize {
    text.chars().count()
}

// ============================================================================
// Tables (upstream const tables, verbatim)
// ============================================================================}

fn symbols() -> BTreeMap<&'static str, &'static str> {
    [
        ("alpha", "α"),
        ("beta", "β"),
        ("gamma", "γ"),
        ("delta", "δ"),
        ("epsilon", "ϵ"),
        ("varepsilon", "ε"),
        ("zeta", "ζ"),
        ("eta", "η"),
        ("theta", "θ"),
        ("vartheta", "ϑ"),
        ("iota", "ι"),
        ("kappa", "κ"),
        ("varkappa", "ϰ"),
        ("lambda", "λ"),
        ("mu", "μ"),
        ("nu", "ν"),
        ("xi", "ξ"),
        ("pi", "π"),
        ("varpi", "ϖ"),
        ("rho", "ρ"),
        ("varrho", "ϱ"),
        ("sigma", "σ"),
        ("varsigma", "ς"),
        ("tau", "τ"),
        ("upsilon", "υ"),
        ("phi", "ϕ"),
        ("varphi", "φ"),
        ("chi", "χ"),
        ("psi", "ψ"),
        ("omega", "ω"),
        ("Gamma", "Γ"),
        ("Delta", "Δ"),
        ("Theta", "Θ"),
        ("Lambda", "Λ"),
        ("Xi", "Ξ"),
        ("Pi", "Π"),
        ("Sigma", "Σ"),
        ("Upsilon", "Υ"),
        ("Phi", "Φ"),
        ("Psi", "Ψ"),
        ("Omega", "Ω"),
        ("pm", "±"),
        ("mp", "∓"),
        ("times", "×"),
        ("div", "÷"),
        ("cdot", "·"),
        ("ast", "∗"),
        ("star", "⋆"),
        ("circ", "∘"),
        ("bullet", "•"),
        ("oplus", "⊕"),
        ("ominus", "⊖"),
        ("otimes", "⊗"),
        ("oslash", "⊘"),
        ("odot", "⊙"),
        ("bigcirc", "○"),
        ("dagger", "†"),
        ("ddagger", "‡"),
        ("amalg", "⨿"),
        ("uplus", "⊎"),
        ("sqcap", "⊓"),
        ("sqcup", "⊔"),
        ("triangleleft", "◁"),
        ("triangleright", "▷"),
        ("wr", "≀"),
        ("cap", "∩"),
        ("cup", "∪"),
        ("bigcap", "⋂"),
        ("bigcup", "⋃"),
        ("bigwedge", "⋀"),
        ("bigvee", "⋁"),
        ("bigsqcup", "⨆"),
        ("biguplus", "⨄"),
        ("bigoplus", "⨁"),
        ("bigotimes", "⨂"),
        ("bigodot", "⨀"),
        ("setminus", "∖"),
        ("in", "∈"),
        ("notin", "∉"),
        ("ni", "∋"),
        ("subset", "⊂"),
        ("supset", "⊃"),
        ("subseteq", "⊆"),
        ("supseteq", "⊇"),
        ("sqsubset", "⊏"),
        ("sqsupset", "⊐"),
        ("sqsubseteq", "⊑"),
        ("sqsupseteq", "⊒"),
        ("prec", "≺"),
        ("preceq", "≼"),
        ("succ", "≻"),
        ("succeq", "≽"),
        ("ll", "≪"),
        ("gg", "≫"),
        ("le", "≤"),
        ("leq", "≤"),
        ("leqslant", "≤"),
        ("ge", "≥"),
        ("geq", "≥"),
        ("geqslant", "≥"),
        ("ne", "≠"),
        ("neq", "≠"),
        ("equiv", "≡"),
        ("approx", "≈"),
        ("sim", "∼"),
        ("simeq", "≃"),
        ("cong", "≅"),
        ("asymp", "≍"),
        ("doteq", "≐"),
        ("propto", "∝"),
        ("parallel", "∥"),
        ("perp", "⊥"),
        ("mid", "∣"),
        ("vdash", "⊢"),
        ("dashv", "⊣"),
        ("models", "⊨"),
        ("Vdash", "⊩"),
        ("Vvdash", "⊪"),
        ("nvdash", "⊬"),
        ("nvDash", "⊭"),
        ("forall", "∀"),
        ("exists", "∃"),
        ("nexists", "∄"),
        ("neg", "¬"),
        ("land", "∧"),
        ("wedge", "∧"),
        ("lor", "∨"),
        ("vee", "∨"),
        ("to", "→"),
        ("rightarrow", "→"),
        ("rightarrow", "→"),
        ("longrightarrow", "→"),
        ("leftarrow", "←"),
        ("longleftarrow", "←"),
        ("gets", "←"),
        ("leftrightarrow", "↔"),
        ("longleftrightarrow", "↔"),
        ("hookleftarrow", "↩"),
        ("hookrightarrow", "↪"),
        ("twoheadleftarrow", "↞"),
        ("twoheadrightarrow", "↠"),
        ("leftharpoonup", "↼"),
        ("leftharpoondown", "↽"),
        ("rightharpoonup", "⇀"),
        ("rightharpoondown", "⇁"),
        ("rightleftharpoons", "⇌"),
        ("leftrightharpoons", "⇋"),
        ("nearrow", "↗"),
        ("searrow", "↘"),
        ("swarrow", "↙"),
        ("nwarrow", "↖"),
        ("rightsquigarrow", "⇝"),
        ("leadsto", "⇝"),
        ("Rightarrow", "⇒"),
        ("Longrightarrow", "⇒"),
        ("Leftarrow", "⇐"),
        ("Longleftarrow", "⇐"),
        ("Leftrightarrow", "⇔"),
        ("Longleftrightarrow", "⇔"),
        ("implies", "⇒"),
        ("iff", "⇔"),
        ("mapsto", "↦"),
        ("longmapsto", "↦"),
        ("uparrow", "↑"),
        ("downarrow", "↓"),
        ("partial", "∂"),
        ("nabla", "∇"),
        ("int", "∫"),
        ("iint", "∬"),
        ("iiint", "∭"),
        ("oint", "∮"),
        ("sum", "∑"),
        ("prod", "∏"),
        ("coprod", "∐"),
        ("infty", "∞"),
        ("emptyset", "∅"),
        ("varnothing", "∅"),
        ("angle", "∠"),
        ("therefore", "∴"),
        ("because", "∵"),
        ("aleph", "ℵ"),
        ("beth", "ℶ"),
        ("gimel", "ℷ"),
        ("daleth", "ℸ"),
        ("top", "⊤"),
        ("bot", "⊥"),
        ("triangle", "△"),
        ("square", "□"),
        ("lozenge", "◊"),
        ("checkmark", "✓"),
        ("complement", "∁"),
        ("wp", "℘"),
        ("prime", "′"),
        ("ldots", "…"),
        ("dots", "…"),
        ("cdots", "⋯"),
        ("vdots", "⋮"),
        ("ddots", "⋱"),
        ("ell", "ℓ"),
        ("hbar", "ℏ"),
        ("Im", "ℑ"),
        ("Re", "ℜ"),
        ("langle", "⟨"),
        ("rangle", "⟩"),
        ("vert", "|"),
        ("lvert", "|"),
        ("rvert", "|"),
        ("Vert", "‖"),
        ("lVert", "‖"),
        ("rVert", "‖"),
        ("lbrace", "{"),
        ("rbrace", "}"),
        ("backslash", "\\"),
        ("lfloor", "⌊"),
        ("rfloor", "⌋"),
        ("lceil", "⌈"),
        ("rceil", "⌉"),
        ("colon", ":"),
    ]
    .into_iter()
    .collect()
}

const NAMED_OPERATORS: [&str; 32] = [
    "arccos", "arcsin", "arctan", "arg", "cos", "cosh", "cot", "coth", "csc", "deg", "det", "dim",
    "exp", "gcd", "hom", "inf", "ker", "lg", "lim", "liminf", "limsup", "ln", "log", "max", "min",
    "Pr", "sec", "sin", "sinh", "sup", "tan", "tanh",
];

const LIMIT_OPERATORS: [&str; 11] = [
    "argmax", "argmin", "inf", "injlim", "lim", "liminf", "limsup", "max", "min", "projlim", "sup",
];

const DISPLAY_LIMIT_SYMBOLS: [&str; 16] = [
    "bigcap",
    "bigcup",
    "bigodot",
    "bigoplus",
    "bigotimes",
    "bigsqcup",
    "biguplus",
    "bigvee",
    "bigwedge",
    "coprod",
    "int",
    "iint",
    "iiint",
    "oint",
    "prod",
    "sum",
];

const RELATION_COMMANDS: [&str; 81] = [
    "Leftarrow",
    "Leftrightarrow",
    "Longleftarrow",
    "Longleftrightarrow",
    "Longrightarrow",
    "Rightarrow",
    "Vdash",
    "Vvdash",
    "approx",
    "asymp",
    "cong",
    "dashv",
    "doteq",
    "downarrow",
    "equiv",
    "ge",
    "geq",
    "geqslant",
    "gets",
    "gg",
    "hookleftarrow",
    "hookrightarrow",
    "iff",
    "implies",
    "in",
    "leadsto",
    "le",
    "leftarrow",
    "leftharpoondown",
    "leftharpoonup",
    "leftrightarrow",
    "leftrightharpoons",
    "leq",
    "leqslant",
    "ll",
    "longleftarrow",
    "longleftrightarrow",
    "longmapsto",
    "longrightarrow",
    "mapsto",
    "mid",
    "models",
    "ne",
    "nearrow",
    "neq",
    "ni",
    "notin",
    "nvdash",
    "nvDash",
    "nwarrow",
    "parallel",
    "perp",
    "prec",
    "preceq",
    "propto",
    "rightharpoondown",
    "rightharpoonup",
    "rightleftharpoons",
    "rightarrow",
    "rightsquigarrow",
    "searrow",
    "sim",
    "simeq",
    "sqsubset",
    "sqsubseteq",
    "sqsupset",
    "sqsupseteq",
    "subset",
    "subseteq",
    "succ",
    "succeq",
    "supset",
    "supseteq",
    "swarrow",
    "to",
    "triangleleft",
    "triangleright",
    "twoheadleftarrow",
    "twoheadrightarrow",
    "uparrow",
    "vdash",
];

fn negated_symbols() -> BTreeMap<&'static str, &'static str> {
    [
        ("<", "≮"),
        (">", "≯"),
        ("=", "≠"),
        ("∈", "∉"),
        ("∋", "∌"),
        ("∣", "∤"),
        ("∥", "∦"),
        ("∼", "≁"),
        ("≃", "≄"),
        ("≅", "≇"),
        ("≈", "≉"),
        ("≡", "≢"),
        ("≤", "≰"),
        ("≥", "≱"),
        ("≺", "⊀"),
        ("≻", "⊁"),
        ("⊂", "⊄"),
        ("⊃", "⊅"),
        ("⊆", "⊈"),
        ("⊇", "⊉"),
        ("⊢", "⊬"),
        ("⊨", "⊭"),
        ("↔", "↮"),
        ("←", "↚"),
        ("→", "↛"),
        ("⇒", "⇏"),
        ("⇐", "⇍"),
        ("⇔", "⇎"),
        ("≼", "⋠"),
        ("≽", "⋡"),
    ]
    .into_iter()
    .collect()
}

fn blackboard() -> BTreeMap<&'static str, &'static str> {
    [
        ("C", "ℂ"),
        ("H", "ℍ"),
        ("N", "ℕ"),
        ("P", "ℙ"),
        ("Q", "ℚ"),
        ("R", "ℝ"),
        ("Z", "ℤ"),
    ]
    .into_iter()
    .collect()
}

fn superscripts() -> BTreeMap<&'static str, &'static str> {
    [
        ("0", "⁰"),
        ("1", "¹"),
        ("2", "²"),
        ("3", "³"),
        ("4", "⁴"),
        ("5", "⁵"),
        ("6", "⁶"),
        ("7", "⁷"),
        ("8", "⁸"),
        ("9", "⁹"),
        ("+", "⁺"),
        ("-", "⁻"),
        ("=", "⁼"),
        ("(", "⁽"),
        (")", "⁾"),
        ("a", "ᵃ"),
        ("b", "ᵇ"),
        ("c", "ᶜ"),
        ("d", "ᵈ"),
        ("e", "ᵉ"),
        ("f", "ᶠ"),
        ("g", "ᵍ"),
        ("h", "ʰ"),
        ("i", "ⁱ"),
        ("j", "ʲ"),
        ("k", "ᵏ"),
        ("l", "ˡ"),
        ("m", "ᵐ"),
        ("n", "ⁿ"),
        ("o", "ᵒ"),
        ("p", "ᵖ"),
        ("r", "ʳ"),
        ("s", "ˢ"),
        ("t", "ᵗ"),
        ("u", "ᵘ"),
        ("v", "ᵛ"),
        ("w", "ʷ"),
        ("x", "ˣ"),
        ("y", "ʸ"),
        ("z", "ᶻ"),
    ]
    .into_iter()
    .collect()
}

fn subscripts() -> BTreeMap<&'static str, &'static str> {
    [
        ("0", "₀"),
        ("1", "₁"),
        ("2", "₂"),
        ("3", "₃"),
        ("4", "₄"),
        ("5", "₅"),
        ("6", "₆"),
        ("7", "₇"),
        ("8", "₈"),
        ("9", "₉"),
        ("+", "₊"),
        ("-", "₋"),
        ("=", "₌"),
        ("(", "₍"),
        (")", "₎"),
        ("a", "ₐ"),
        ("e", "ₑ"),
        ("h", "ₕ"),
        ("i", "ᵢ"),
        ("j", "ⱼ"),
        ("k", "ₖ"),
        ("l", "ₗ"),
        ("m", "ₘ"),
        ("n", "ₙ"),
        ("o", "ₒ"),
        ("p", "ₚ"),
        ("r", "ᵣ"),
        ("s", "ₛ"),
        ("t", "ₜ"),
        ("u", "ᵤ"),
        ("v", "ᵥ"),
        ("x", "ₓ"),
    ]
    .into_iter()
    .collect()
}

const SPACING_COMMANDS: [&str; 12] = [
    ",",
    ":",
    ";",
    " ",
    ">",
    "enspace",
    "enskip",
    "medspace",
    "quad",
    "qquad",
    "thickspace",
    "thinspace",
];

const NEGATIVE_SPACING_COMMANDS: [&str; 4] = ["!", "negmedspace", "negthickspace", "negthinspace"];

const NEGATIVE_SPACE: &str = "\u{0}";

const IGNORED_COMMANDS: [&str; 6] = [
    "displaystyle",
    "limits",
    "nolimits",
    "scriptstyle",
    "scriptscriptstyle",
    "textstyle",
];

const SIZE_COMMANDS: [&str; 12] = [
    "big", "Big", "bigg", "Bigg", "bigl", "Bigl", "biggl", "Biggl", "bigr", "Bigr", "biggr",
    "Biggr",
];

const PLAIN_WRAPPERS: [&str; 31] = [
    "emph",
    "mathcal",
    "mathbf",
    "mathfrak",
    "mathit",
    "mathrm",
    "mathnormal",
    "mathscr",
    "mathsf",
    "mathtt",
    "mathup",
    "mbox",
    "overbrace",
    "pmb",
    "smash",
    "substack",
    "text",
    "textbf",
    "textit",
    "textmd",
    "textnormal",
    "textrm",
    "textsc",
    "textsf",
    "textsl",
    "texttt",
    "textup",
    "underbrace",
    "bm",
    "boldsymbol",
    "fontencoding",
];

fn accents() -> BTreeMap<&'static str, &'static str> {
    [
        ("acute", "\u{301}"),
        ("bar", "\u{305}"),
        ("breve", "\u{306}"),
        ("check", "\u{30c}"),
        ("ddot", "\u{308}"),
        ("dot", "\u{307}"),
        ("grave", "\u{300}"),
        ("hat", "\u{302}"),
        ("mathring", "\u{30a}"),
        ("overleftarrow", "\u{20d6}"),
        ("overleftrightarrow", "\u{20e1}"),
        ("overline", "\u{305}"),
        ("overrightarrow", "\u{20d7}"),
        ("tilde", "\u{303}"),
        ("underline", "\u{332}"),
        ("vec", "\u{20d7}"),
        ("widehat", "\u{302}"),
        ("widetilde", "\u{303}"),
    ]
    .into_iter()
    .collect()
}

const NAMED_OPERATOR_START: &str = "\u{f0004}";
const NAMED_OPERATOR_END: &str = "\u{f0005}";
const LAYOUT_MARKER_START: &str = "\u{f0000}";
const LAYOUT_MARKER_END: &str = "\u{f0001}";
const PROTECTED_SPACE: &str = "\u{f0002}";

// ============================================================================
// Formatting helpers
// ============================================================================}

fn is_word_char(ch: char) -> bool {
    ch.is_alphanumeric()
}

fn replace_characters(value: &str, replacements: &BTreeMap<&str, &str>) -> Option<String> {
    let mut result = String::new();
    for character in value.chars() {
        let key = character.to_string();
        let replacement = replacements.get(key.as_str())?;
        result.push_str(replacement);
    }
    Some(result)
}

/// Collapse spacing around = +- and map to Unicode scripts (upstream
/// `formatScript`); falls back to `_<value>` / `^(value)`.
fn format_script(value: &str, kind: &str) -> String {
    let value = value.trim();
    // Upstream: value.replace(/\s*([=+-])\s*/g, "$1").
    let mut collapsed = String::with_capacity(value.len());
    let chars: Vec<char> = value.chars().collect();
    for (index, ch) in chars.iter().enumerate() {
        if (*ch == '=' || *ch == '+' || *ch == '-')
            && index > 0
            && index + 1 < chars.len()
            && chars[index - 1].is_whitespace()
            && chars[index + 1].is_whitespace()
        {
            continue;
        }
        collapsed.push(*ch);
    }
    let collapsed = collapsed.trim();
    let replacements = if kind == "sub" {
        subscripts()
    } else {
        superscripts()
    };
    if let Some(unicode) = replace_characters(collapsed, &replacements) {
        return unicode;
    }
    let prefix = if kind == "sub" { "_" } else { "^" };
    if collapsed.chars().count() == 1
        || (kind == "sub" && collapsed.chars().all(|c| c.is_ascii_alphabetic()))
    {
        format!("{prefix}{collapsed}")
    } else {
        format!("{prefix}({collapsed})")
    }
}

fn is_simple_numerator(value: &str) -> bool {
    value.chars().all(|c| is_word_char(c) || c == '.')
}

fn format_fraction(numerator: &str, denominator: &str) -> String {
    let numerator = numerator.trim();
    let denominator = denominator.trim();
    let simple_numerator = is_simple_numerator(numerator);
    let simple_denominator =
        denominator.chars().all(|c| c.is_numeric() || c == '.') || denominator.chars().count() == 1;
    format!(
        "{}/{simple_denominator_wrap}",
        if simple_numerator {
            numerator.to_string()
        } else {
            format!("({numerator})")
        },
        simple_denominator_wrap = if simple_denominator {
            denominator.to_string()
        } else {
            format!("({denominator})")
        }
    )
}

fn format_root(value: &str, symbol: &str) -> String {
    let value = value.trim();
    if is_simple_numerator(value) {
        format!("{symbol}{value}")
    } else {
        format!("{symbol}({value})")
    }
}

/// Strip the named-operator markers, adding spacing around them (upstream
/// `normalizeOutput`).
fn normalize_output(value: &str) -> String {
    // NAMED_OPERATOR_LEFT_SPACING_PATTERN: marker-start after word char.
    let mut step1 = String::with_capacity(value.len());
    let chars: Vec<char> = value.chars().collect();
    for i in 0..chars.len() {
        if chars[i] == '\u{f0004}' {
            let previous_is_word = i > 0
                && (chars[i - 1].is_alphanumeric()
                    || chars[i - 1] == ')'
                    || chars[i - 1] == ']'
                    || chars[i - 1] == '}'
                    || chars[i - 1] == '\u{f0001}');
            if previous_is_word {
                step1.push(' ');
            }
            continue;
        }
        step1.push(chars[i]);
    }
    // Remove all marker-start occurrences (the spacing insert above
    // already consumed the ones adjacent to word chars; upstream removes
    // every remaining marker-start too).
    let step1 = step1.replace(NAMED_OPERATOR_START, "");
    // NAMED_OPERATOR_RIGHT_SPACING_PATTERN: marker-end before word char /
    // √ / marker-start.
    let mut step2 = String::with_capacity(step1.len());
    let chars: Vec<char> = step1.chars().collect();
    for i in 0..chars.len() {
        if chars[i] == '\u{f0005}' {
            let next_is_word = chars
                .get(i + 1)
                .map(|c| c.is_alphanumeric() || *c == '√' || *c == '\u{f0000}')
                .unwrap_or(false);
            if next_is_word {
                step2.push(' ');
            }
            continue;
        }
        step2.push(chars[i]);
    }
    let without_end = step2.replace(NAMED_OPERATOR_END, "");
    // Per-line collapse + trim, drop empty lines except interior ones.
    let lines: Vec<&str> = without_end.split('\n').collect();
    let mut kept: Vec<String> = Vec::new();
    for (index, line) in lines.iter().enumerate() {
        let collapsed: String = collapse_whitespace(line);
        let trimmed = collapsed.trim();
        if !trimmed.is_empty() || (index > 0 && index < lines.len() - 1) {
            kept.push(trimmed.to_string());
        } else {
            kept.push(String::new());
        }
    }
    let joined = kept.join("\n");
    joined.trim().to_string()
}

fn collapse_whitespace(line: &str) -> String {
    let mut out = String::with_capacity(line.len());
    let mut last_was_space = false;
    for ch in line.chars() {
        if ch == ' ' || ch == '\t' {
            if !last_was_space {
                out.push(' ');
            }
            last_was_space = true;
        } else {
            out.push(ch);
            last_was_space = false;
        }
    }
    out
}

// ============================================================================
// Layout
// ============================================================================}

#[derive(Debug, Clone)]
enum LayoutNode {
    Fraction {
        numerator: String,
        denominator: String,
    },
    Operator {
        operator: String,
        lower: Option<String>,
        upper: Option<String>,
    },
    Matrix {
        lines: Vec<String>,
        baseline: usize,
    },
}

impl LayoutNode {
    fn kind(&self) -> &'static str {
        match self {
            LayoutNode::Fraction { .. } => "fraction",
            LayoutNode::Operator { .. } => "operator",
            LayoutNode::Matrix { .. } => "matrix",
        }
    }
}

#[derive(Debug, Clone)]
struct Layout {
    lines: Vec<String>,
    width: usize,
    baseline: usize,
}

fn pad_layout_line(line: &str, width: usize, centered: bool) -> String {
    let padding = width.saturating_sub(visible_width(line));
    let left = if centered { padding / 2 } else { 0 };
    format!("{}{}{}", " ".repeat(left), line, " ".repeat(padding - left))
}

fn join_layouts(layouts: &[Layout]) -> Layout {
    if layouts.is_empty() {
        return Layout {
            lines: vec![String::new()],
            width: 0,
            baseline: 0,
        };
    }
    let baseline = layouts.iter().map(|l| l.baseline).max().unwrap_or(0);
    let below = layouts
        .iter()
        .map(|l| l.lines.len().saturating_sub(l.baseline + 1))
        .max()
        .unwrap_or(0);
    let mut lines = Vec::new();
    for row in 0..=(baseline + below) {
        let mut line = String::new();
        for layout in layouts {
            let source_row = row as isize - baseline as isize + layout.baseline as isize;
            if source_row >= 0 && (source_row as usize) < layout.lines.len() {
                line.push_str(&pad_layout_line(
                    &layout.lines[source_row as usize],
                    layout.width,
                    false,
                ));
            } else {
                line.push_str(&" ".repeat(layout.width));
            }
        }
        lines.push(line.trim_end().to_string());
    }
    Layout {
        lines,
        width: layouts.iter().map(|l| l.width).sum(),
        baseline,
    }
}

/// Parse `\u{f0000}<index>\u{f0001}` markers starting at `from`.
fn find_layout_marker(source: &str, from: usize) -> Option<(usize, usize, usize)> {
    // returns (match_start, match_end_exclusive, index)
    let bytes: Vec<char> = source.chars().collect();
    let mut i = from;
    // convert char index to char positions
    let start_marker: Vec<char> = LAYOUT_MARKER_START.chars().collect();
    let end_marker: Vec<char> = LAYOUT_MARKER_END.chars().collect();
    while i < bytes.len() {
        if bytes[i] == start_marker[0] {
            // try match
            let mut j = i;
            let mut ok = true;
            for mc in &start_marker {
                if j >= bytes.len() || bytes[j] != *mc {
                    ok = false;
                    break;
                }
                j += 1;
            }
            if ok {
                let digits_start = j;
                while j < bytes.len() && bytes[j].is_ascii_digit() {
                    j += 1;
                }
                let digits_end = j;
                for mc in &end_marker {
                    if j >= bytes.len() || bytes[j] != *mc {
                        ok = false;
                        break;
                    }
                    j += 1;
                }
                if ok {
                    let index: usize = bytes[digits_start..digits_end]
                        .iter()
                        .collect::<String>()
                        .parse()
                        .unwrap_or(usize::MAX);
                    return Some((i, j, index));
                }
            }
        }
        i += 1;
    }
    None
}

fn render_layout(source: &str, nodes: &[LayoutNode]) -> Layout {
    let mut rendered_lines: Vec<String> = Vec::new();
    let mut first_baseline = 0usize;
    for source_line in source.split('\n') {
        let mut layouts: Vec<Layout> = Vec::new();
        let mut position = 0usize;
        let mut previous_node_kind: Option<&'static str> = None;
        let mut cursor = 0usize;
        while let Some((match_start, match_end, index)) = find_layout_marker(source_line, cursor) {
            let node = nodes.get(index);
            let Some(node) = node else {
                cursor = match_end;
                continue;
            };
            if match_start > position {
                let sliced: String = source_line
                    .chars()
                    .skip(position)
                    .take(match_start - position)
                    .collect();
                let leading_ws = sliced.starts_with(char::is_whitespace);
                let trailing_ws = sliced.ends_with(char::is_whitespace);
                let trimmed = if previous_node_kind.is_some() {
                    sliced.trim_start()
                } else {
                    sliced.as_str()
                }
                .trim_end();
                let preserve_leading = previous_node_kind == Some("matrix") && leading_ws;
                let preserve_trailing = node.kind() == "matrix" && trailing_ws;
                let text = if !trimmed.is_empty() {
                    format!(
                        "{}{trimmed}{}",
                        if preserve_leading { " " } else { "" },
                        if preserve_trailing { " " } else { "" }
                    )
                } else if preserve_leading || preserve_trailing {
                    " ".to_string()
                } else {
                    String::new()
                };
                layouts.push(Layout {
                    lines: vec![text.clone()],
                    width: visible_width(&text),
                    baseline: 0,
                });
            }
            match node {
                LayoutNode::Fraction {
                    numerator,
                    denominator,
                } => {
                    let numerator_layout = render_layout(numerator, nodes);
                    let denominator_layout = render_layout(denominator, nodes);
                    let content_width = numerator_layout.width.max(denominator_layout.width).max(1);
                    let width = content_width + 2;
                    let mut lines: Vec<String> = numerator_layout
                        .lines
                        .iter()
                        .map(|line| pad_layout_line(line, width, true))
                        .collect();
                    lines.push(format!(" {} ", "─".repeat(content_width)));
                    lines.extend(
                        denominator_layout
                            .lines
                            .iter()
                            .map(|line| pad_layout_line(line, width, true)),
                    );
                    layouts.push(Layout {
                        lines,
                        width,
                        baseline: numerator_layout.lines.len(),
                    });
                }
                LayoutNode::Operator {
                    operator,
                    lower,
                    upper,
                } => {
                    let content_width = visible_width(operator)
                        .max(lower.as_deref().map(visible_width).unwrap_or(0))
                        .max(upper.as_deref().map(visible_width).unwrap_or(0));
                    let mut lines: Vec<String> = Vec::new();
                    if let Some(upper) = upper {
                        lines.push(format!("{} ", pad_layout_line(upper, content_width, true)));
                    }
                    lines.push(format!(
                        "{} ",
                        pad_layout_line(operator, content_width, true)
                    ));
                    if let Some(lower) = lower {
                        lines.push(format!("{} ", pad_layout_line(lower, content_width, true)));
                    }
                    layouts.push(Layout {
                        lines,
                        width: content_width + 1,
                        baseline: if upper.is_none() { 0 } else { 1 },
                    });
                }
                LayoutNode::Matrix { lines, baseline } => {
                    let width = lines.iter().map(|l| visible_width(l)).max().unwrap_or(0);
                    layouts.push(Layout {
                        lines: lines
                            .iter()
                            .map(|l| pad_layout_line(l, width, false))
                            .collect(),
                        width,
                        baseline: *baseline,
                    });
                }
            }
            position = match_end;
            previous_node_kind = Some(node.kind());
            cursor = match_end;
        }
        if position < source_line.chars().count() {
            let sliced: String = source_line.chars().skip(position).collect();
            let leading_ws = sliced.starts_with(char::is_whitespace);
            let trimmed = if previous_node_kind.is_some() {
                sliced.trim_start()
            } else {
                sliced.as_str()
            };
            let text = if previous_node_kind == Some("matrix") && leading_ws {
                format!(" {trimmed}")
            } else {
                trimmed.to_string()
            };
            layouts.push(Layout {
                lines: vec![text.clone()],
                width: visible_width(&text),
                baseline: 0,
            });
        }
        let line_layout = join_layouts(&layouts);
        if rendered_lines.is_empty() {
            first_baseline = line_layout.baseline;
        }
        rendered_lines.extend(line_layout.lines);
    }
    Layout {
        width: rendered_lines
            .iter()
            .map(|l| visible_width(l))
            .max()
            .unwrap_or(0),
        lines: rendered_lines,
        baseline: first_baseline,
    }
}

// ============================================================================
// Parser
// ============================================================================}

struct LatexParser {
    #[allow(dead_code)]
    source: String,
    chars: Vec<char>,
    layout_nodes: Vec<LayoutNode>,
    display: bool,
    position: usize,
    supported: bool,
    stack_fractions: bool,
}

impl LatexParser {
    fn new(source: String, display: bool) -> Self {
        let chars: Vec<char> = source.chars().collect();
        Self {
            source,
            chars,
            layout_nodes: Vec::new(),
            display,
            position: 0,
            supported: true,
            stack_fractions: true,
        }
    }

    fn current(&self) -> Option<char> {
        self.chars.get(self.position).copied()
    }

    fn source_len_chars(&self) -> usize {
        self.chars.len()
    }

    fn slice_char(&self, start: usize, end: usize) -> String {
        self.chars[start..end.min(self.chars.len())]
            .iter()
            .collect()
    }

    fn render(&mut self) -> Option<String> {
        let rendered = self.parse_sequence(None);
        if !self.supported || self.position != self.source_len_chars() {
            return None;
        }
        Some(normalize_output(&rendered))
    }

    fn parse_sequence(&mut self, end_character: Option<char>) -> String {
        let mut result = String::new();
        while self.position < self.source_len_chars() {
            let Some(character) = self.current() else {
                break;
            };
            if let Some(end) = end_character
                && character == end
            {
                self.position += 1;
                return result;
            }
            if character == '}' {
                self.supported = false;
                return result;
            }
            if character == '{' {
                self.position += 1;
                result.push_str(&self.parse_sequence(Some('}')));
                continue;
            }
            if character == '\\' {
                let command = self.parse_command();
                if command == NEGATIVE_SPACE {
                    result = result.trim_end().to_string();
                    if result.ends_with(NAMED_OPERATOR_END) {
                        result.truncate(result.len() - NAMED_OPERATOR_END.len());
                    }
                } else {
                    result.push_str(&command);
                }
                continue;
            }
            if character == '^' || character == '_' {
                self.position += 1;
                result = result.trim_end().to_string();
                let script = format_script(
                    &self.parse_required_argument(false),
                    if character == '_' { "sub" } else { "sup" },
                );
                if result.ends_with(NAMED_OPERATOR_END) {
                    result = format!(
                        "{}{script}{NAMED_OPERATOR_END}",
                        &result[..result.len() - NAMED_OPERATOR_END.len()]
                    );
                } else {
                    result.push_str(&script);
                }
                continue;
            }
            if character.is_whitespace() {
                result.push_str(&self.parse_whitespace());
                continue;
            }
            if character == '=' || character == '<' || character == '>' {
                result = format!("{} {character} ", result.trim_end());
                self.position += 1;
                continue;
            }
            if character == '&' {
                self.position += 1;
                continue;
            }
            if character == '~' {
                self.position += 1;
                result.push(' ');
                continue;
            }
            if character == '.' {
                // Trailing layout marker: append '.' to the last matrix line.
                let marker = trailing_layout_marker(&result);
                if let Some(index) = marker {
                    let is_matrix = self
                        .layout_nodes
                        .get(index)
                        .map(|n| matches!(n, LayoutNode::Matrix { .. }))
                        .unwrap_or(false);
                    if is_matrix {
                        if let Some(LayoutNode::Matrix { lines, .. }) =
                            self.layout_nodes.get_mut(index)
                        {
                            let last = lines.len() - 1;
                            if let Some(line) = lines.get_mut(last) {
                                line.push('.');
                            }
                        }
                        self.position += 1;
                        continue;
                    }
                }
            }
            result.push(character);
            self.position += 1;
        }
        if end_character.is_some() {
            self.supported = false;
        }
        result
    }

    fn parse_whitespace(&mut self) -> String {
        while self.position < self.source_len_chars()
            && self.current().is_some_and(|c| c.is_whitespace())
        {
            self.position += 1;
        }
        " ".to_string()
    }

    fn parse_command(&mut self) -> String {
        self.position += 1;
        if self.position >= self.source_len_chars() {
            self.supported = false;
            return String::new();
        }

        let first = self.current().unwrap_or('\0');
        if first == '\n' || first == '\r' {
            self.position += 1;
            if first == '\r' && self.current() == Some('\n') {
                self.position += 1;
            }
            return " ".to_string();
        }
        let command = if first.is_ascii_alphabetic() {
            let start = self.position;
            while self.position < self.source_len_chars()
                && self.current().is_some_and(|c| c.is_ascii_alphabetic())
            {
                self.position += 1;
            }
            self.slice_char(start, self.position)
        } else {
            self.position += 1;
            first.to_string()
        };

        if command == "\\" {
            return "\n".to_string();
        }
        if SPACING_COMMANDS.contains(&command.as_str()) {
            return " ".to_string();
        }
        if NEGATIVE_SPACING_COMMANDS.contains(&command.as_str()) {
            return NEGATIVE_SPACE.to_string();
        }
        if IGNORED_COMMANDS.contains(&command.as_str()) {
            return String::new();
        }
        if matches!(command.as_str(), "{" | "}" | "$" | "%" | "#" | "_" | "&") {
            return command;
        }
        if command == "|" {
            return "‖".to_string();
        }
        if command == "not" {
            let value = self.parse_required_argument(false).trim().to_string();
            let negated = negated_symbols().get(value.as_str()).copied();
            if let Some(negated) = negated {
                return format!(" {negated} ");
            }
            let characters: Vec<char> = value.chars().collect();
            if characters.is_empty() {
                self.supported = false;
                return String::new();
            }
            return format!(" {}{} ", characters[0], '\u{338}');
        }
        if LIMIT_OPERATORS.contains(&command.as_str()) {
            return self.parse_operator(&command, true, true, false);
        }

        let symbol = symbols().get(command.as_str()).copied();
        if let Some(symbol) = symbol {
            if DISPLAY_LIMIT_SYMBOLS.contains(&command.as_str()) {
                return self.parse_operator(symbol, false, true, false);
            }
            return if command == "cdot"
                || command == "times"
                || RELATION_COMMANDS.contains(&command.as_str())
            {
                format!(" {symbol} ")
            } else {
                symbol.to_string()
            };
        }
        if NAMED_OPERATORS.contains(&command.as_str()) {
            return format!("{NAMED_OPERATOR_START}{command}{NAMED_OPERATOR_END}");
        }
        if SIZE_COMMANDS.contains(&command.as_str()) {
            return String::new();
        }
        if matches!(command.as_str(), "left" | "middle" | "right") {
            if self.current() == Some('.') {
                self.position += 1;
            }
            return String::new();
        }
        if matches!(command.as_str(), "frac" | "dfrac" | "tfrac") {
            let should_stack = self.display && self.stack_fractions && command != "tfrac";
            let numerator = self.parse_required_argument(!should_stack);
            let denominator = self.parse_required_argument(!should_stack);
            if should_stack {
                self.layout_nodes.push(LayoutNode::Fraction {
                    numerator: normalize_output(&numerator),
                    denominator: normalize_output(&denominator),
                });
                let index = self.layout_nodes.len() - 1;
                return format!("{LAYOUT_MARKER_START}{index}{LAYOUT_MARKER_END}");
            }
            return format_fraction(&numerator, &denominator);
        }
        if command == "sqrt" {
            let degree = self.parse_optional_argument().map(|d| d.trim().to_string());
            let value = self.parse_required_argument(true);
            match degree.as_deref() {
                None | Some("2") => return format_root(&value, "√"),
                Some("3") => return format_root(&value, "∛"),
                Some("4") => return format_root(&value, "∜"),
                Some(other) => {
                    return format!(
                        "{}{}",
                        format_script(other, "sup"),
                        format_root(&value, "√")
                    );
                }
            }
        }
        if matches!(command.as_str(), "boxed" | "fbox") {
            return format!("[{}]", self.parse_required_argument(true).trim());
        }
        if matches!(command.as_str(), "binom" | "dbinom" | "tbinom") {
            let a = self.parse_required_argument(true);
            let b = self.parse_required_argument(true);
            return format!("({a} choose {b})");
        }
        let accent = accents().get(command.as_str()).copied();
        if let Some(accent) = accent {
            let value = self.parse_required_argument(true);
            return if value.chars().count() == 1 {
                format!("{value}{accent}")
            } else {
                format!("{command}({value})")
            };
        }
        if command == "mathbb" {
            let value = self.parse_required_argument(true);
            let blackboard = blackboard();
            return value
                .chars()
                .map(|character| {
                    blackboard
                        .get(&character.to_string().as_str())
                        .copied()
                        .unwrap_or(character.to_string().as_str())
                        .to_string()
                })
                .collect();
        }
        if command == "operatorname" {
            let starred = self.current() == Some('*');
            if starred {
                self.position += 1;
            }
            let operator = normalize_output(&self.parse_required_argument(true))
                .trim()
                .to_string();
            return self.parse_operator(&operator, true, starred, false);
        }
        if matches!(command.as_str(), "mod" | "bmod") {
            return " mod ".to_string();
        }
        if matches!(command.as_str(), "pmod" | "pod") {
            let value = self.parse_required_argument(true).trim().to_string();
            return if command == "pmod" {
                format!(" (mod {value})")
            } else {
                format!(" ({value})")
            };
        }
        if matches!(command.as_str(), "overset" | "stackrel") {
            let upper = self.parse_required_argument(true);
            let value = self.parse_required_argument(true).trim().to_string();
            return format!("{value}{}", format_script(&upper, "sup"));
        }
        if command == "underset" {
            let lower = self.parse_required_argument(true);
            let value = self.parse_required_argument(true).trim().to_string();
            return format!("{value}{}", format_script(&lower, "sub"));
        }
        if PLAIN_WRAPPERS.contains(&command.as_str()) {
            let value = self.parse_required_argument(true);
            return if command.starts_with("text") || command == "mbox" {
                value
            } else {
                value.trim().to_string()
            };
        }
        if command == "begin" {
            return self.parse_environment();
        }
        if command == "end" {
            self.supported = false;
            return String::new();
        }

        self.supported = false;
        format!("\\{command}")
    }

    fn parse_operator(
        &mut self,
        operator: &str,
        inline_lower_bracket: bool,
        display_limits: bool,
        spaced: bool,
    ) -> String {
        let mut use_display_limits = display_limits;
        // Look for \limits / \nolimits modifiers.
        while self.position < self.source_len_chars()
            && matches!(self.current(), Some(' ') | Some('\t'))
        {
            self.position += 1;
        }
        if self.current() == Some('\\') {
            let save = self.position;
            self.position += 1;
            let start = self.position;
            while self.position < self.source_len_chars()
                && self.current().is_some_and(|c| c.is_ascii_alphabetic())
            {
                self.position += 1;
            }
            let word = self.slice_char(start, self.position);
            if word == "limits" {
                use_display_limits = true;
            } else if word == "nolimits" {
                use_display_limits = false;
            } else {
                self.position = save;
            }
        }

        let mut lower: Option<String> = None;
        let mut upper: Option<String> = None;
        loop {
            let mut script_position = self.position;
            while script_position < self.source_len_chars()
                && matches!(self.chars.get(script_position), Some(' ') | Some('\t'))
            {
                script_position += 1;
            }
            let kind = self.chars.get(script_position).copied();
            if !matches!(kind, Some('_') | Some('^')) {
                break;
            }
            self.position = script_position + 1;
            let value = normalize_output(&self.parse_required_argument(false)).replace(' ', "");
            if kind == Some('_') {
                if lower.is_some() {
                    self.supported = false;
                }
                lower = Some(value);
            } else {
                if upper.is_some() {
                    self.supported = false;
                }
                upper = Some(value);
            }
        }

        if self.display && use_display_limits && (lower.is_some() || upper.is_some()) {
            self.layout_nodes.push(LayoutNode::Operator {
                operator: operator.to_string(),
                lower,
                upper,
            });
            let index = self.layout_nodes.len() - 1;
            return format!("{LAYOUT_MARKER_START}{index}{LAYOUT_MARKER_END}");
        }

        let mut rendered = operator.to_string();
        if let Some(lower) = lower {
            rendered.push_str(&if inline_lower_bracket {
                format!("[{lower}]")
            } else {
                format_script(&lower, "sub")
            });
        }
        if let Some(upper) = upper {
            rendered.push_str(&format_script(&upper, "sup"));
        }
        if spaced {
            format!(" {rendered} ")
        } else {
            rendered
        }
    }

    fn parse_required_argument(&mut self, stack_fractions: bool) -> String {
        let previous = self.stack_fractions;
        self.stack_fractions = previous && stack_fractions;
        let value = self.parse_required_argument_value();
        self.stack_fractions = previous;
        value
    }

    fn parse_required_argument_value(&mut self) -> String {
        while self.position < self.source_len_chars()
            && self.current().is_some_and(|c| c.is_whitespace())
        {
            self.position += 1;
        }
        if self.position >= self.source_len_chars() {
            self.supported = false;
            return String::new();
        }
        if self.current() == Some('{') {
            self.position += 1;
            return self.parse_sequence(Some('}'));
        }
        if self.current() == Some('\\') {
            return self.parse_command();
        }
        let value = self.current().unwrap_or('\0');
        self.position += 1;
        value.to_string()
    }

    fn parse_optional_argument(&mut self) -> Option<String> {
        while self.position < self.source_len_chars()
            && matches!(self.current(), Some(' ') | Some('\t'))
        {
            self.position += 1;
        }
        if self.current() != Some('[') {
            return None;
        }
        // Find the matching ']' from the current position.
        let mut end = self.position + 1;
        while end < self.source_len_chars() && self.chars[end] != ']' {
            end += 1;
        }
        if end >= self.source_len_chars() {
            self.supported = false;
            return None;
        }
        let value = self.slice_char(self.position + 1, end);
        self.position = end + 1;
        Some(self.render_nested(&value, true))
    }

    fn read_raw_group(&mut self) -> Option<String> {
        while self.position < self.source_len_chars()
            && matches!(self.current(), Some(' ') | Some('\t'))
        {
            self.position += 1;
        }
        if self.current() != Some('{') {
            self.supported = false;
            return None;
        }
        self.position += 1;
        let start = self.position;
        let mut depth = 1usize;
        while self.position < self.source_len_chars() {
            let character = self.current().unwrap_or('\0');
            if character == '\\' {
                self.position += 2;
                continue;
            }
            if character == '{' {
                depth += 1;
            }
            if character == '}' {
                depth -= 1;
            }
            if depth == 0 {
                let value = self.slice_char(start, self.position);
                self.position += 1;
                return Some(value);
            }
            self.position += 1;
        }
        self.supported = false;
        None
    }

    fn split_environment_rows(body: &str) -> Vec<String> {
        // Upstream: body.split(/\\\\(?:\[[^\]\n]*\])?/)
        let mut rows = Vec::new();
        let mut current = String::new();
        let chars: Vec<char> = body.chars().collect();
        let mut i = 0;
        while i < chars.len() {
            if chars[i] == '\\' && i + 1 < chars.len() && chars[i + 1] == '\\' {
                // Optional [..] size argument.
                let mut j = i + 2;
                if j < chars.len() && chars[j] == '[' {
                    while j < chars.len() && chars[j] != ']' && chars[j] != '\n' {
                        j += 1;
                    }
                    if j < chars.len() && chars[j] == ']' {
                        j += 1;
                    }
                }
                rows.push(std::mem::take(&mut current));
                i = j;
                continue;
            }
            current.push(chars[i]);
            i += 1;
        }
        rows.push(current);
        rows
    }

    fn parse_environment(&mut self) -> String {
        let Some(environment) = self.read_raw_group() else {
            return String::new();
        };
        let end_marker = format!("\\end{{{environment}}}");
        let end_marker_chars: Vec<char> = end_marker.chars().collect();
        // Find the end marker in the remaining source.
        let mut end: Option<usize> = None;
        let mut i = self.position;
        while i + end_marker_chars.len() <= self.source_len_chars() {
            if self.chars[i..i + end_marker_chars.len()] == end_marker_chars[..] {
                end = Some(i);
                break;
            }
            i += 1;
        }
        let Some(end) = end else {
            self.supported = false;
            return String::new();
        };
        let body = self.slice_char(self.position, end);
        self.position = end + end_marker_chars.len();

        if matches!(
            environment.as_str(),
            "equation" | "equation*" | "displaymath"
        ) {
            return self.render_nested(&body, true).trim().to_string();
        }

        if matches!(
            environment.as_str(),
            "aligned"
                | "align"
                | "align*"
                | "alignedat"
                | "alignat"
                | "alignat*"
                | "gather"
                | "gathered"
                | "multline"
                | "multline*"
                | "split"
        ) {
            let aligned_at = matches!(environment.as_str(), "alignedat" | "alignat" | "alignat*");
            let aligned_body = if aligned_at {
                // Strip a leading {..} argument.
                let trimmed = body.trim_start();
                if trimmed.starts_with('{') {
                    if let Some(close) = trimmed.find('}') {
                        trimmed[close + 1..].to_string()
                    } else {
                        body.clone()
                    }
                } else {
                    body.clone()
                }
            } else {
                body.clone()
            };
            return Self::split_environment_rows(&aligned_body)
                .iter()
                .map(|row| {
                    let cells: Vec<&str> = row.split('&').collect();
                    let source = if aligned_at {
                        cells
                            .chunks(2)
                            .map(|chunk| chunk.concat())
                            .collect::<Vec<_>>()
                            .join(" ")
                    } else {
                        cells.concat()
                    };
                    self.render_nested(&source, true).trim().to_string()
                })
                .filter(|row| !row.is_empty())
                .collect::<Vec<_>>()
                .join("\n");
        }

        if matches!(environment.as_str(), "cases" | "cases*") {
            let rows: Vec<Vec<String>> = Self::split_environment_rows(&body)
                .iter()
                .map(|row| {
                    row.split('&')
                        .map(|cell| self.render_nested(cell, false).trim().to_string())
                        .collect()
                })
                .filter(|row: &Vec<String>| row.iter().any(|cell| !cell.is_empty()))
                .collect();
            return rows
                .iter()
                .enumerate()
                .map(|(index, row)| {
                    let value = row
                        .first()
                        .map(|v| v.trim_end_matches(',').trim_end().to_string())
                        .unwrap_or_default();
                    let condition = row.get(1).cloned().unwrap_or_default();
                    let delimiter = if index == 0 {
                        "⎧"
                    } else if index == rows.len() - 1 {
                        "⎩"
                    } else {
                        "⎨"
                    };
                    let condition_prefix = if condition.starts_with("if")
                        || condition.starts_with("when")
                        || condition.starts_with("for")
                        || condition.starts_with("otherwise")
                        || condition.starts_with("If")
                        || condition.starts_with("When")
                        || condition.starts_with("For")
                        || condition.starts_with("Otherwise")
                    {
                        " "
                    } else {
                        " if "
                    };
                    if condition.is_empty() {
                        format!("{delimiter} {value}")
                    } else {
                        format!("{delimiter} {value}{condition_prefix}{condition}")
                    }
                })
                .collect::<Vec<_>>()
                .join("\n");
        }

        if matches!(
            environment.as_str(),
            "array"
                | "matrix"
                | "smallmatrix"
                | "pmatrix"
                | "bmatrix"
                | "Bmatrix"
                | "vmatrix"
                | "Vmatrix"
        ) {
            let matrix_body = if environment == "array" {
                let trimmed = body.trim_start();
                if trimmed.starts_with('{') {
                    if let Some(close) = trimmed.find('}') {
                        trimmed[close + 1..].to_string()
                    } else {
                        body.clone()
                    }
                } else {
                    body.clone()
                }
            } else {
                body.clone()
            };
            return self.render_matrix(&environment, &matrix_body);
        }

        self.supported = false;
        body
    }

    fn render_matrix(&mut self, environment: &str, body: &str) -> String {
        let matrix: Vec<Vec<String>> = Self::split_environment_rows(body)
            .iter()
            .map(|row| {
                row.split('&')
                    .map(|cell| self.render_nested(cell, false).trim().to_string())
                    .collect::<Vec<String>>()
            })
            .filter(|row| row.iter().any(|cell| !cell.is_empty()))
            .collect();
        let column_count = matrix.iter().map(|row| row.len()).max().unwrap_or(0);
        let column_widths: Vec<usize> = (0..column_count)
            .map(|column| {
                matrix
                    .iter()
                    .map(|row| visible_width(row.get(column).map(String::as_str).unwrap_or("")))
                    .max()
                    .unwrap_or(0)
            })
            .collect();
        let rows: Vec<String> = matrix
            .iter()
            .map(|row| {
                (0..column_count)
                    .map(|column| {
                        let cell = row.get(column).map(String::as_str).unwrap_or("");
                        let pad = column_widths[column].saturating_sub(visible_width(cell));
                        format!("{cell}{}", PROTECTED_SPACE.repeat(pad))
                    })
                    .collect::<Vec<_>>()
                    .join(" │ ")
            })
            .collect();

        let lines: Vec<String> = if matches!(environment, "array" | "matrix" | "smallmatrix") {
            rows
        } else {
            let delimiters: BTreeMap<&str, [&str; 6]> = BTreeMap::from([
                ("pmatrix", ["⎛", "⎞", "⎜", "⎟", "⎝", "⎠"]),
                ("bmatrix", ["⎡", "⎤", "⎢", "⎥", "⎣", "⎦"]),
                ("Bmatrix", ["⎧", "⎫", "⎨", "⎬", "⎩", "⎭"]),
                ("vmatrix", ["│", "│", "│", "│", "│", "│"]),
                ("Vmatrix", ["║", "║", "║", "║", "║", "║"]),
            ]);
            let Some(delimiter) = delimiters.get(environment) else {
                self.supported = false;
                return rows.join("\n");
            };
            rows.iter()
                .enumerate()
                .map(|(index, row)| {
                    let left = if index == 0 {
                        delimiter[0]
                    } else if index == rows.len() - 1 {
                        delimiter[4]
                    } else {
                        delimiter[2]
                    };
                    let right = if index == 0 {
                        delimiter[1]
                    } else if index == rows.len() - 1 {
                        delimiter[5]
                    } else {
                        delimiter[3]
                    };
                    format!("{left} {row} {right}")
                })
                .collect()
        };

        if lines.len() <= 1 {
            return lines.first().cloned().unwrap_or_default();
        }
        self.layout_nodes
            .push(LayoutNode::Matrix { lines, baseline: 0 });
        let index = self.layout_nodes.len() - 1;
        format!("{LAYOUT_MARKER_START}{index}{LAYOUT_MARKER_END}")
    }

    fn render_nested(&mut self, source: &str, stack_fractions: bool) -> String {
        // The nested parser owns a fresh node list appended to the shared
        // one after rendering; marker indices are rebased by the base so
        // they stay valid in the outer layout pass.
        let base = self.layout_nodes.len();
        let mut parser = LatexParser::new(source.to_string(), self.display && stack_fractions);
        let rendered = parser.render().map(|r| rebase_layout_markers(&r, base));
        self.layout_nodes.extend(parser.layout_nodes);
        match rendered {
            Some(rendered) => rendered,
            None => {
                self.supported = false;
                source.to_string()
            }
        }
    }
}

/// Rebase layout marker indices by adding `base` to each index.
fn rebase_layout_markers(source: &str, base: usize) -> String {
    if base == 0 {
        return source.to_string();
    }
    let mut out = String::with_capacity(source.len());
    let chars: Vec<char> = source.chars().collect();
    let start_marker: Vec<char> = LAYOUT_MARKER_START.chars().collect();
    let end_marker: Vec<char> = LAYOUT_MARKER_END.chars().collect();
    let mut i = 0;
    while i < chars.len() {
        if chars[i] == start_marker[0] {
            let mut j = i;
            let mut ok = true;
            for mc in &start_marker {
                if j >= chars.len() || chars[j] != *mc {
                    ok = false;
                    break;
                }
                j += 1;
            }
            if ok {
                let digits_start = j;
                while j < chars.len() && chars[j].is_ascii_digit() {
                    j += 1;
                }
                for mc in &end_marker {
                    if j >= chars.len() || chars[j] != *mc {
                        ok = false;
                        break;
                    }
                    j += 1;
                }
                if ok {
                    let digits: String = chars[digits_start..j - 1].iter().collect();
                    let index: usize = digits.parse().unwrap_or(0);
                    out.push_str(LAYOUT_MARKER_START);
                    out.push_str(&(index + base).to_string());
                    out.push_str(LAYOUT_MARKER_END);
                    i = j;
                    continue;
                }
            }
        }
        out.push(chars[i]);
        i += 1;
    }
    out
}

fn trailing_layout_marker(result: &str) -> Option<usize> {
    // TRAILING_LAYOUT_MARKER_PATTERN: \u{f0000}(\d+)\u{f0001}$
    let chars: Vec<char> = result.chars().collect();
    if chars.len() < 3 || chars[chars.len() - 1] != '\u{f0001}' {
        return None;
    }
    let mut i = chars.len() - 2;
    while i > 0 && chars[i].is_ascii_digit() {
        i -= 1;
    }
    if chars[i] != '\u{f0000}' || i + 1 >= chars.len() - 1 {
        return None;
    }
    let digits: String = chars[i + 1..chars.len() - 1].iter().collect();
    if digits.is_empty() {
        return None;
    }
    digits.parse().ok()
}

// ============================================================================
// Public API
// ============================================================================}

/// Options for [`render_latex`] (upstream `RenderLatexOptions`).
#[derive(Debug, Clone, Copy, Default)]
pub struct RenderLatexOptions {
    /// Stack fractions and operator limits vertically for display math.
    pub display: bool,
}

/// Render a basic LaTeX math expression as terminal-friendly Unicode
/// text (upstream `renderLatex`). Returns None when the expression
/// contains unsupported or malformed syntax.
pub fn render_latex(source: &str, options: RenderLatexOptions) -> Option<String> {
    let mut parser = LatexParser::new(source.to_string(), options.display);
    let rendered = parser.render()?;
    if parser.layout_nodes.is_empty() {
        return Some(rendered.replace(PROTECTED_SPACE, " "));
    }
    let lines = render_layout(&rendered, &parser.layout_nodes).lines;
    let indentation = lines
        .iter()
        .filter(|line| !line.trim().is_empty())
        .map(|line| line.len() - line.trim_start().len())
        .min()
        .unwrap_or(0);
    Some(
        lines
            .iter()
            .map(|line| {
                let mut skip = indentation;
                let mut result = String::new();
                for ch in line.chars() {
                    if skip > 0 {
                        skip -= 1;
                        continue;
                    }
                    result.push(ch);
                }
                result.trim_end().to_string()
            })
            .collect::<Vec<_>>()
            .join("\n")
            .trim_end()
            .replace(PROTECTED_SPACE, " "),
    )
}
