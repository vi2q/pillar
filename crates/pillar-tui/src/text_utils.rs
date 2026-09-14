//! Port of packages/tui/src/utils.ts (pi v0.84.3), the text-metrics core:
//! ANSI/OSC/APC escape extraction, terminal visible width (with the
//! string-width-compatible grapheme classification), ANSI SGR state
//! tracking across line breaks, word wrapping that preserves ANSI codes,
//! and background application.
//!
//! divergences: `Intl.Segmenter` grapheme clustering and the `v`-flag
//! Unicode property regexes become per-character classification
//! (`unicode_width`-style tables trimmed to the classes the TUI needs:
//! combining marks, zero-width/control, East Asian wide/fullwidth, and
//! RGI-emoji ranges); the width cache is a simple map.

use std::collections::BTreeMap;
use std::sync::Mutex;

// ============================================================================
// ANSI code extraction (upstream extractAnsiCode)
// ============================================================================}

/// An extracted escape sequence.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AnsiCode {
    pub code: String,
    pub length: usize,
}

/// Extract an ANSI/OSC/APC escape sequence at `pos` (upstream
/// `extractAnsiCode`): CSI (`ESC [ ... m/G/K/H/J`), OSC (`ESC ] ... BEL`
/// or `ESC \`), APC (`ESC _ ... BEL` or `ESC \`), and other 2-char
/// escapes.
pub fn extract_ansi_code(chars: &[char], pos: usize) -> Option<AnsiCode> {
    if pos >= chars.len() || chars[pos] != '\u{1b}' {
        return None;
    }
    let next = *chars.get(pos + 1)?;
    // CSI sequence.
    if next == '[' {
        let mut j = pos + 2;
        while j < chars.len() && !matches!(chars[j], 'm' | 'G' | 'K' | 'H' | 'J') {
            j += 1;
        }
        if j < chars.len() {
            return Some(AnsiCode {
                code: chars[pos..=j].iter().collect(),
                length: j + 1 - pos,
            });
        }
        return None;
    }
    // OSC sequence.
    if next == ']' {
        let mut j = pos + 2;
        while j < chars.len() {
            if chars[j] == '\u{7}' {
                return Some(AnsiCode {
                    code: chars[pos..=j].iter().collect(),
                    length: j + 1 - pos,
                });
            }
            if chars[j] == '\u{1b}' && chars.get(j + 1) == Some(&'\\') {
                return Some(AnsiCode {
                    code: chars[pos..=j + 1].iter().collect(),
                    length: j + 2 - pos,
                });
            }
            j += 1;
        }
        return None;
    }
    // APC sequence.
    if next == '_' {
        let mut j = pos + 2;
        while j < chars.len() {
            if chars[j] == '\u{7}' {
                return Some(AnsiCode {
                    code: chars[pos..=j].iter().collect(),
                    length: j + 1 - pos,
                });
            }
            if chars[j] == '\u{1b}' && chars.get(j + 1) == Some(&'\\') {
                return Some(AnsiCode {
                    code: chars[pos..=j + 1].iter().collect(),
                    length: j + 2 - pos,
                });
            }
            j += 1;
        }
        return None;
    }
    // Other 2-char escapes.
    Some(AnsiCode {
        code: chars[pos..pos + 2].iter().collect(),
        length: 2,
    })
}

/// Strip ANSI/OSC/APC sequences while preserving visible text (upstream
/// `stripTerminalSequences`).
pub fn strip_terminal_sequences(text: &str) -> String {
    if !text.contains('\u{1b}') {
        return text.to_string();
    }
    let chars: Vec<char> = text.chars().collect();
    let mut result = String::with_capacity(text.len());
    let mut i = 0;
    while i < chars.len() {
        match extract_ansi_code(&chars, i) {
            Some(ansi) => i += ansi.length,
            None => {
                result.push(chars[i]);
                i += 1;
            }
        }
    }
    result
}

fn is_printable_ascii(text: &str) -> bool {
    text.chars().all(|c| ('\u{20}'..='\u{7e}').contains(&c))
}

// ============================================================================
// Width classification
// ============================================================================}

/// Zero-width: default-ignorable, control, mark, surrogate (upstream
/// `zeroWidthRegex`).
fn is_zero_width(ch: char) -> bool {
    ch.is_control() || is_mark_char(ch) || is_default_ignorable(ch)
}

fn is_default_ignorable(ch: char) -> bool {
    matches!(
        ch as u32,
        0x00AD | 0x034F | 0x061C | 0x115F..=0x1160 | 0x17B4 | 0x17B5 | 0x180B..=0x180F
            | 0x200B..=0x200F | 0x202A..=0x202E | 0x2060..=0x206F | 0x3164 | 0xFE00..=0xFE0F
            | 0xFEFF | 0xFFA0 | 0xFFF0..=0xFFF8 | 0x1BCA0..=0x1BCA3 | 0x1D173..=0x1D17A
            | 0xE0000..=0xE0FFF
    )
}

/// Combining marks (upstream `\p{Mark}`): Mn + Mc + Me ranges relevant to
/// terminals.
fn is_mark_char(ch: char) -> bool {
    matches!(ch as u32,
        0x0300..=0x036F | 0x0483..=0x0489 | 0x0591..=0x05BD | 0x0610..=0x061A
        | 0x064B..=0x065F | 0x0670 | 0x06D6..=0x06DC | 0x06DF..=0x06E4 | 0x0730..=0x074A
        | 0x07A6..=0x07B0 | 0x0900..=0x0903 | 0x093A..=0x093C | 0x0941..=0x0948
        | 0x094D | 0x0951..=0x0957 | 0x0E31 | 0x0E34..=0x0E3A | 0x0E47..=0x0E4E
        | 0x20D0..=0x20F0 | 0xFE00..=0xFE0F | 0xFE20..=0xFE2F
    )
}

/// Spacing marks that terminals allocate cells for (upstream
/// `terminalSpacingMarkRegex`).
fn is_terminal_spacing_mark(ch: char) -> bool {
    // Spacing_Mark (Mc) minus U+1734 U+302E U+302F, plus the legacy
    // wcwidth exceptions.
    let is_mc = matches!(ch as u32,
        0x0903 | 0x093B | 0x093E..=0x0940 | 0x0949..=0x094C | 0x094E..=0x094F
        | 0x0982..=0x0983 | 0x09BF..=0x09C0 | 0x09C7..=0x09C8 | 0x0A03
        | 0x0A3E..=0x0A40 | 0x0ABE..=0x0AC0 | 0x0B3F | 0x0BBF | 0x0BC1..=0x0BC2
        | 0x0BE3..=0x0BE5 | 0x0CBF | 0x0D41..=0x0D44 | 0x102B..=0x1038
        | 0x103B..=0x103E | 0x105E..=0x1060 | 0x1715 | 0x1734 | 0x17B6
        | 0x18A9 | 0x1923..=0x1926 | 0x19B0..=0x19C0 | 0x1A19..=0x1A1A
        | 0x1B04 | 0x1B35 | 0x1B3B | 0x302E | 0x302F
    );
    if matches!(ch as u32, 0x1734 | 0x302E | 0x302F) {
        return false;
    }
    is_mc
        || matches!(ch as u32, 0x065F | 0x0F7F | 0x102B | 0x102C | 0x1031 | 0x1033..=0x1035
            | 0x1038 | 0x103A..=0x103E)
}

/// Whether a leading codepoint could be an emoji (upstream
/// `couldBeEmoji`).
fn could_be_emoji(first: char, len: usize) -> bool {
    let cp = first as u32;
    (0x1F000..=0x1FBFF).contains(&cp)
        || (0x2300..=0x23FF).contains(&cp)
        || (0x2600..=0x27BF).contains(&cp)
        || (0x2B50..=0x2B55).contains(&cp)
        || first == '\u{FE0F}'
        || len > 2
}

/// Is this an RGI emoji sequence? The port checks the bounded set of
/// ZWJ/VS16/keycap sequences (upstream `\p{RGI_Emoji}`).
fn is_rgi_emoji(chars: &[char]) -> bool {
    if chars.is_empty() {
        return false;
    }
    let cp = chars[0] as u32;
    let in_emoji_block = (0x1F000..=0x1FAFF).contains(&cp)
        || (0x2600..=0x27BF).contains(&cp)
        || (0x2B00..=0x2BFF).contains(&cp)
        || (0x1F1E6..=0x1F1FF).contains(&cp);
    if !in_emoji_block {
        return false;
    }
    // RGI: base emoji optionally followed by VS16, ZWJ sequences, and
    // keycap/tag modifiers.
    chars.iter().all(|c| {
        let c = *c as u32;
        (0x1F000..=0x1FAFF).contains(&c)
            || (0x2600..=0x27BF).contains(&c)
            || (0x2B00..=0x2BFF).contains(&c)
            || (0x1F1E6..=0x1F1FF).contains(&c)
            || c == 0xFE0F
            || c == 0x200D
            || c == 0x20E3
            || c == 0x2028
            || c == 0x20E3
            || (0x0300..=0x036F).contains(&c)
            || c == 0x2764
            || c == 0x270B
            || c == 0x270C
    })
}

/// East Asian Width for the classes terminals render as 2 cells (upstream
/// `eastAsianWidth` returning W/F for these ranges).
fn east_asian_width_two_cells(ch: char) -> bool {
    matches!(ch as u32,
        0x1100..=0x115F | 0x2E80..=0x303E | 0x3041..=0x33FF | 0x3400..=0x4DBF
        | 0x4E00..=0x9FFF | 0xA000..=0xA4CF | 0xA960..=0xA97F | 0xAC00..=0xD7A3
        | 0xF900..=0xFAFF | 0xFE10..=0xFE19 | 0xFE30..=0xFE6F | 0xFF00..=0xFF60
        | 0xFFE0..=0xFFE6 | 0x1F300..=0x1F64F | 0x1F900..=0x1F9FF
        | 0x17000..=0x187FF | 0x1B000..=0x1B16F | 0x20000..=0x2FFFD
        | 0x30000..=0x3FFFD
    )
}

/// CJK characters that allow a line break after them (upstream
/// `cjkBreakRegex`).
pub fn is_cjk_break(ch: char) -> bool {
    matches!(ch as u32,
        0x2E80..=0x2EFF | 0x3000..=0x303F | 0x3041..=0x309F | 0x30A0..=0x30FF
        | 0x3130..=0x318F | 0x31A0..=0x31BF | 0x3400..=0x4DBF | 0x4E00..=0x9FFF
        | 0xAC00..=0xD7A3 | 0xF900..=0xFAFF | 0xFF66..=0xFF9D | 0x1B000..=0x1B16F
        | 0x20000..=0x2A6DF
    )
}

/// Non-printing characters stripped before width computation (upstream
/// `leadingNonPrintingRegex` / `nonPrintingCharRegex`).
fn is_non_printing(ch: char) -> bool {
    ch.is_control()
        || is_mark_char(ch)
        || is_default_ignorable(ch)
        || matches!(ch as u32, 0x0600..=0x0605 | 0x061C | 0x06DD | 0x070F | 0x08E2 | 0x200B..=0x200F | 0x202A..=0x202E | 0x2060..=0x2064 | 0x2066..=0x206F | 0xFEFF | 0xFFF9..=0xFFFB)
}

/// Terminal width of a single grapheme (upstream `graphemeWidth`).
pub fn grapheme_width(segment: &str) -> usize {
    if segment == "\t" {
        return 3;
    }
    let chars: Vec<char> = segment.chars().collect();
    if chars.is_empty() {
        return 0;
    }
    // Terminal-spacing marks occupy cells even without a base.
    if chars.iter().all(|c| is_terminal_spacing_mark(*c)) {
        return chars.len();
    }
    // Zero-width clusters.
    if chars.iter().all(|c| is_zero_width(*c)) {
        return 0;
    }
    // Emoji (with pre-filter).
    if could_be_emoji(chars[0], chars.len()) && is_rgi_emoji(&chars) {
        return 2;
    }
    // Base visible codepoint: strip leading non-printing.
    let base: &[char] = {
        let mut start = 0;
        while start < chars.len() && is_non_printing(chars[start]) {
            start += 1;
        }
        &chars[start..]
    };
    let Some(base_ch) = base.first().copied() else {
        return 0;
    };
    // Regional indicators render as full-width emoji.
    if (0x1F1E6..=0x1F1FF).contains(&(base_ch as u32)) {
        return 2;
    }
    let mut width = if east_asian_width_two_cells(base_ch) {
        2
    } else {
        1
    };
    // Count trailing visible code points terminals may allocate cells for.
    let mut follows_mark = false;
    for ch in base.iter().skip(1) {
        if is_terminal_spacing_mark(*ch) {
            width += 1;
            follows_mark = false;
        } else if is_mark_char(*ch) {
            follows_mark = true;
        } else if !is_non_printing(*ch) {
            let c = *ch as u32;
            if follows_mark || (0xFF00..=0xFFEF).contains(&c) {
                width += if east_asian_width_two_cells(*ch) {
                    2
                } else {
                    1
                };
            } else if c == 0x0E33 || c == 0x0EB3 {
                width += 1;
            }
            follows_mark = false;
        }
    }
    width
}

// ============================================================================
// visibleWidth
// ============================================================================}

const WIDTH_CACHE_SIZE: usize = 512;

fn width_cache() -> &'static Mutex<BTreeMap<String, usize>> {
    static CACHE: std::sync::OnceLock<Mutex<BTreeMap<String, usize>>> = std::sync::OnceLock::new();
    CACHE.get_or_init(|| Mutex::new(BTreeMap::new()))
}

/// Calculate the visible width of a string in terminal columns (upstream
/// `visibleWidth`): tabs are 3 columns, escape sequences don't count, and
/// each grapheme contributes its terminal width. An LRU-bounded cache
/// mirrors upstream.
pub fn visible_width(text: &str) -> usize {
    if text.is_empty() {
        return 0;
    }
    if is_printable_ascii(text) {
        return text.chars().count();
    }
    if let Some(cached) = width_cache().lock().unwrap().get(text) {
        return *cached;
    }
    let mut clean = text.to_string();
    if clean.contains('\t') {
        clean = clean.replace('\t', "   ");
    }
    let chars: Vec<char> = clean.chars().collect();
    let stripped: String = if clean.contains('\u{1b}') {
        let mut out = String::with_capacity(clean.len());
        let mut i = 0;
        while i < chars.len() {
            match extract_ansi_code(&chars, i) {
                Some(ansi) => i += ansi.length,
                None => {
                    out.push(chars[i]);
                    i += 1;
                }
            }
        }
        out
    } else {
        clean
    };
    // Grapheme clustering: the port treats each char as its own cluster
    // (divergence: Intl.Segmenter merges combining sequences — the width
    // computation compensates via mark handling below).
    let mut width = 0usize;
    let mut pending: Vec<char> = Vec::new();
    for ch in stripped.chars() {
        // Start a new cluster when the char is not a mark/zero-width.
        if is_mark_char(ch) || is_default_ignorable(ch) && pending.is_empty() {
            pending.push(ch);
            continue;
        }
        if pending.is_empty() {
            pending.push(ch);
            continue;
        }
        width += grapheme_width(&pending.iter().collect::<String>());
        pending = vec![ch];
    }
    if !pending.is_empty() {
        width += grapheme_width(&pending.iter().collect::<String>());
    }
    let mut cache = width_cache().lock().unwrap();
    if cache.len() >= WIDTH_CACHE_SIZE {
        if let Some(first) = cache.keys().next().cloned() {
            cache.remove(&first);
        }
    }
    cache.insert(text.to_string(), width);
    width
}

/// Split text into grapheme-ish clusters (upstream
/// `graphemeSegmenter.segment`): base chars with following marks. The
/// width computation treats each cluster as one unit.
pub fn grapheme_clusters(text: &str) -> Vec<String> {
    let mut clusters: Vec<String> = Vec::new();
    for ch in text.chars() {
        if is_mark_char(ch) || is_terminal_spacing_mark(ch) {
            if let Some(last) = clusters.last_mut() {
                last.push(ch);
                continue;
            }
        }
        clusters.push(ch.to_string());
    }
    clusters
}

// ============================================================================
// OSC 8 hyperlinks
// ============================================================================}

#[derive(Debug, Clone, PartialEq, Eq)]
enum HyperlinkTerminator {
    Bel,
    St,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ActiveHyperlink {
    params: String,
    url: String,
    terminator: HyperlinkTerminator,
}

fn parse_osc8_hyperlink(ansi_code: &str) -> Option<Option<ActiveHyperlink>> {
    if !ansi_code.starts_with("\u{1b}]8;") {
        return None;
    }
    let terminator = if ansi_code.ends_with('\u{7}') {
        HyperlinkTerminator::Bel
    } else {
        HyperlinkTerminator::St
    };
    let body = &ansi_code[4..ansi_code.len()
        - match terminator {
            HyperlinkTerminator::Bel => 1,
            HyperlinkTerminator::St => 2,
        }];
    let Some(separator_index) = body.find(';') else {
        return Some(None);
    };
    let params = body[..separator_index].to_string();
    let url = body[separator_index + 1..].to_string();
    if url.is_empty() {
        return Some(None);
    }
    Some(Some(ActiveHyperlink {
        params,
        url,
        terminator,
    }))
}

fn format_osc8_hyperlink(hyperlink: &ActiveHyperlink) -> String {
    let terminator = match hyperlink.terminator {
        HyperlinkTerminator::Bel => "\u{7}".to_string(),
        HyperlinkTerminator::St => "\u{1b}\\".to_string(),
    };
    format!(
        "\u{1b}]8;{};{}{}",
        hyperlink.params, hyperlink.url, terminator
    )
}

fn format_osc8_close(terminator: &HyperlinkTerminator) -> String {
    match terminator {
        HyperlinkTerminator::Bel => "\u{1b}]8;;\u{7}".to_string(),
        HyperlinkTerminator::St => "\u{1b}]8;;\u{1b}\\".to_string(),
    }
}

pub fn get_active_osc8_close(prefix: &str) -> String {
    if !prefix.contains("\u{1b}]8;") {
        return String::new();
    }
    let chars: Vec<char> = prefix.chars().collect();
    let mut active: Option<ActiveHyperlink> = None;
    let mut i = 0;
    while i < chars.len() {
        match extract_ansi_code(&chars, i) {
            Some(ansi) => {
                if let Some(Some(hyperlink)) = parse_osc8_hyperlink(&ansi.code) {
                    active = Some(hyperlink);
                }
                i += ansi.length;
            }
            None => i += 1,
        }
    }
    active
        .map(|h| format_osc8_close(&h.terminator))
        .unwrap_or_default()
}

// ============================================================================
// ANSI SGR tracking
// ============================================================================}

/// Track active ANSI SGR attributes across line breaks (upstream
/// `AnsiCodeTracker`).
#[derive(Debug, Clone, Default)]
pub struct AnsiCodeTracker {
    bold: bool,
    dim: bool,
    italic: bool,
    underline: bool,
    blink: bool,
    inverse: bool,
    hidden: bool,
    strikethrough: bool,
    fg_color: Option<String>,
    bg_color: Option<String>,
    active_hyperlink: Option<ActiveHyperlink>,
}

impl AnsiCodeTracker {
    pub fn new() -> Self {
        Self::default()
    }

    fn reset(&mut self) {
        *self = Self {
            active_hyperlink: self.active_hyperlink.take(),
            ..Self::default()
        };
        self.active_hyperlink = self.active_hyperlink.take();
        // SGR reset does not affect OSC 8 hyperlink state: restore it.
        // (Handled by the take above; re-set for clarity.)
    }

    pub fn clear(&mut self) {
        self.bold = false;
        self.dim = false;
        self.italic = false;
        self.underline = false;
        self.blink = false;
        self.inverse = false;
        self.hidden = false;
        self.strikethrough = false;
        self.fg_color = None;
        self.bg_color = None;
        self.active_hyperlink = None;
    }

    /// Process an escape sequence (upstream `process`).
    pub fn process(&mut self, ansi_code: &str) {
        if let Some(hyperlink) = parse_osc8_hyperlink(ansi_code) {
            self.active_hyperlink = hyperlink;
            return;
        }
        if !ansi_code.ends_with('m') {
            return;
        }
        // Extract the parameters between ESC[ and m.
        let inner = ansi_code.trim_start_matches("\u{1b}[");
        let params_str = inner.trim_end_matches('m');
        if params_str.is_empty() || params_str == "0" {
            self.reset();
            return;
        }
        let parts: Vec<&str> = params_str.split(';').collect();
        let mut i = 0;
        while i < parts.len() {
            let Ok(code) = parts[i].parse::<u32>() else {
                i += 1;
                continue;
            };
            if code == 38 || code == 48 {
                if parts.get(i + 1) == Some(&"5") && parts.get(i + 2).is_some() {
                    let color_code = format!("{};{};{}", parts[i], parts[i + 1], parts[i + 2]);
                    if code == 38 {
                        self.fg_color = Some(color_code);
                    } else {
                        self.bg_color = Some(color_code);
                    }
                    i += 3;
                    continue;
                }
                if parts.get(i + 1) == Some(&"2") && parts.get(i + 4).is_some() {
                    let color_code = format!(
                        "{};{};{};{};{}",
                        parts[i],
                        parts[i + 1],
                        parts[i + 2],
                        parts[i + 3],
                        parts[i + 4]
                    );
                    if code == 38 {
                        self.fg_color = Some(color_code);
                    } else {
                        self.bg_color = Some(color_code);
                    }
                    i += 5;
                    continue;
                }
            }
            match code {
                0 => self.reset(),
                1 => self.bold = true,
                2 => self.dim = true,
                3 => self.italic = true,
                4 => self.underline = true,
                5 => self.blink = true,
                7 => self.inverse = true,
                8 => self.hidden = true,
                9 => self.strikethrough = true,
                21 => self.bold = false,
                22 => {
                    self.bold = false;
                    self.dim = false;
                }
                23 => self.italic = false,
                24 => self.underline = false,
                25 => self.blink = false,
                27 => self.inverse = false,
                28 => self.hidden = false,
                29 => self.strikethrough = false,
                39 => self.fg_color = None,
                49 => self.bg_color = None,
                _ => {
                    if (30..=37).contains(&code) || (90..=97).contains(&code) {
                        self.fg_color = Some(code.to_string());
                    } else if (40..=47).contains(&code) || (100..=107).contains(&code) {
                        self.bg_color = Some(code.to_string());
                    }
                }
            }
            i += 1;
        }
    }

    /// The escape prefix restoring the active state (upstream
    /// `getActiveCodes`).
    pub fn get_active_codes(&self) -> String {
        let mut codes: Vec<&str> = Vec::new();
        if self.bold {
            codes.push("1");
        }
        if self.dim {
            codes.push("2");
        }
        if self.italic {
            codes.push("3");
        }
        if self.underline {
            codes.push("4");
        }
        if self.blink {
            codes.push("5");
        }
        if self.inverse {
            codes.push("7");
        }
        if self.hidden {
            codes.push("8");
        }
        if self.strikethrough {
            codes.push("9");
        }
        if let Some(fg) = &self.fg_color {
            codes.push(fg);
        }
        if let Some(bg) = &self.bg_color {
            codes.push(bg);
        }
        let mut result = if codes.is_empty() {
            String::new()
        } else {
            format!("\u{1b}[{}m", codes.join(";"))
        };
        if let Some(hyperlink) = &self.active_hyperlink {
            result.push_str(&format_osc8_hyperlink(hyperlink));
        }
        result
    }

    pub fn has_active_codes(&self) -> bool {
        self.bold
            || self.dim
            || self.italic
            || self.underline
            || self.blink
            || self.inverse
            || self.hidden
            || self.strikethrough
            || self.fg_color.is_some()
            || self.bg_color.is_some()
            || self.active_hyperlink.is_some()
    }

    /// Codes to close attributes that bleed into padding (upstream
    /// `getLineEndReset`): underline off and OSC 8 close.
    pub fn get_line_end_reset(&self) -> String {
        let mut result = String::new();
        if self.underline {
            result.push_str("\u{1b}[24m");
        }
        if let Some(hyperlink) = &self.active_hyperlink {
            result.push_str(&format_osc8_close(&hyperlink.terminator));
        }
        result
    }
}

fn update_tracker_from_text(text: &str, tracker: &mut AnsiCodeTracker) {
    let chars: Vec<char> = text.chars().collect();
    let mut i = 0;
    while i < chars.len() {
        match extract_ansi_code(&chars, i) {
            Some(ansi) => {
                tracker.process(&ansi.code);
                i += ansi.length;
            }
            None => i += 1,
        }
    }
}

// ============================================================================
// Tokenizing + wrapping
// ============================================================================}

/// Split into words keeping ANSI codes attached (upstream
/// `splitIntoTokensWithAnsi`).
fn split_into_tokens_with_ansi(text: &str) -> Vec<String> {
    let mut tokens: Vec<String> = Vec::new();
    let mut current = String::new();
    let mut pending_ansi = String::new();
    let mut current_kind: Option<u8> = None; // 0 space, 1 word
    let chars: Vec<char> = text.chars().collect();
    let mut i = 0;

    while i < chars.len() {
        if let Some(ansi) = extract_ansi_code(&chars, i) {
            pending_ansi.push_str(&ansi.code);
            i += ansi.length;
            continue;
        }
        let mut end = i;
        while end < chars.len() && extract_ansi_code(&chars, end).is_none() {
            end += 1;
        }

        for grapheme in grapheme_clusters(&chars[i..end].iter().collect::<String>()) {
            let segment_is_space = grapheme == " ";
            if !segment_is_space && is_cjk_break(grapheme.chars().next().unwrap_or('\0')) {
                // Flush and emit CJK chars as their own tokens.
                if !current.is_empty() {
                    tokens.push(std::mem::take(&mut current));
                }
                let token = format!("{pending_ansi}{grapheme}");
                pending_ansi.clear();
                tokens.push(token);
                continue;
            }
            let segment_kind = if segment_is_space { 0u8 } else { 1u8 };
            if !current.is_empty() && current_kind != Some(segment_kind) {
                tokens.push(std::mem::take(&mut current));
            }
            if !pending_ansi.is_empty() {
                current.push_str(&pending_ansi);
                pending_ansi.clear();
            }
            current_kind = Some(segment_kind);
            current.push_str(&grapheme);
        }
        i = end;
    }

    if !pending_ansi.is_empty() {
        if !current.is_empty() {
            current.push_str(&pending_ansi);
        } else if let Some(last) = tokens.last_mut() {
            last.push_str(&pending_ansi);
        } else {
            current = pending_ansi;
        }
    }
    if !current.is_empty() {
        tokens.push(current);
    }
    tokens
}

fn break_long_word(word: &str, width: usize, tracker: &mut AnsiCodeTracker) -> Vec<String> {
    let mut lines: Vec<String> = Vec::new();
    let mut current_line = tracker.get_active_codes();
    let mut current_width = 0usize;

    let chars: Vec<char> = word.chars().collect();
    let mut segments: Vec<(bool, String)> = Vec::new(); // (is_ansi, value)
    let mut i = 0;
    while i < chars.len() {
        if let Some(ansi) = extract_ansi_code(&chars, i) {
            segments.push((true, ansi.code));
            i += ansi.length;
        } else {
            let mut end = i;
            while end < chars.len() && extract_ansi_code(&chars, end).is_none() {
                end += 1;
            }
            let portion: String = chars[i..end].iter().collect();
            for cluster in grapheme_clusters(&portion) {
                segments.push((false, cluster));
            }
            i = end;
        }
    }

    for (is_ansi, value) in segments {
        if is_ansi {
            current_line.push_str(&value);
            tracker.process(&value);
            continue;
        }
        if value.is_empty() {
            continue;
        }
        let grapheme_width = visible_width(&value);
        if current_width + grapheme_width > width {
            let line_end_reset = tracker.get_line_end_reset();
            if !line_end_reset.is_empty() {
                current_line.push_str(&line_end_reset);
            }
            lines.push(std::mem::take(&mut current_line));
            current_line = tracker.get_active_codes();
            current_width = 0;
        }
        current_line.push_str(&value);
        current_width += grapheme_width;
    }

    if !current_line.is_empty() {
        lines.push(current_line);
    }
    if lines.is_empty() {
        lines.push(String::new());
    }
    lines
}

fn wrap_single_line(line: &str, width: usize) -> Vec<String> {
    if line.is_empty() {
        return vec![String::new()];
    }
    let visible_length = visible_width(line);
    if visible_length <= width {
        return vec![line.to_string()];
    }

    let mut wrapped: Vec<String> = Vec::new();
    let mut tracker = AnsiCodeTracker::new();
    let tokens = split_into_tokens_with_ansi(line);
    let mut current_line = String::new();
    let mut current_visible_length = 0usize;

    for token in tokens {
        let token_visible_length = visible_width(&token);
        let is_whitespace = token.trim().is_empty();

        if token_visible_length > width && !is_whitespace {
            if !current_line.is_empty() {
                let line_end_reset = tracker.get_line_end_reset();
                if !line_end_reset.is_empty() {
                    current_line.push_str(&line_end_reset);
                }
                wrapped.push(std::mem::take(&mut current_line));
            }
            let broken = break_long_word(&token, width, &mut tracker);
            for broken_line in broken.iter().take(broken.len() - 1) {
                wrapped.push(broken_line.clone());
            }
            current_line = broken.last().cloned().unwrap_or_default();
            current_visible_length = visible_width(&current_line);
            continue;
        }

        let total_needed = current_visible_length + token_visible_length;
        if total_needed > width && current_visible_length > 0 {
            let line_to_wrap = current_line.trim_end().to_string();
            let line_end_reset = tracker.get_line_end_reset();
            if !line_end_reset.is_empty() {
                let mut line = line_to_wrap.clone();
                line.push_str(&line_end_reset);
                wrapped.push(line);
            } else {
                wrapped.push(line_to_wrap);
            }
            if is_whitespace {
                current_line = tracker.get_active_codes();
                current_visible_length = 0;
            } else {
                current_line = format!("{}{}", tracker.get_active_codes(), token);
                current_visible_length = token_visible_length;
            }
        } else {
            current_line.push_str(&token);
            current_visible_length += token_visible_length;
        }
        update_tracker_from_text(&token, &mut tracker);
    }

    if !current_line.is_empty() {
        wrapped.push(current_line);
    }
    if wrapped.is_empty() {
        wrapped.push(String::new());
    }
    wrapped
        .into_iter()
        .map(|l| l.trim_end().to_string())
        .collect()
}

/// Word-wrap text preserving ANSI codes across line breaks (upstream
/// `wrapTextWithAnsi`): no padding, no backgrounds; each line fits
/// within `width` visible columns.
pub fn wrap_text_with_ansi(text: &str, width: usize) -> Vec<String> {
    if text.is_empty() {
        return vec![String::new()];
    }
    let input_lines: Vec<&str> = text.split('\n').collect();
    let mut result: Vec<String> = Vec::new();
    let mut tracker = AnsiCodeTracker::new();
    for input_line in input_lines {
        let prefix = if result.is_empty() {
            String::new()
        } else {
            tracker.get_active_codes()
        };
        let wrapped = wrap_single_line(&format!("{prefix}{input_line}"), width);
        result.extend(wrapped);
        update_tracker_from_text(input_line, &mut tracker);
    }
    if result.is_empty() {
        result.push(String::new());
    }
    result
}

/// Apply a background to a line, padding to full width (upstream
/// `applyBackgroundToLine`).
pub fn apply_background_to_line(
    line: &str,
    width: usize,
    bg_fn: &dyn Fn(&str) -> String,
) -> String {
    let visible_len = visible_width(line);
    let padding_needed = width.saturating_sub(visible_len);
    let with_padding = format!("{line}{}", " ".repeat(padding_needed));
    bg_fn(&with_padding)
}

// ============================================================================
// Truncation (upstream truncateToWidth / truncateFragmentToWidth)
// ============================================================================}

/// Truncate a fragment to a width without an ellipsis (upstream
/// `truncateFragmentToWidth`).
fn truncate_fragment_to_width(text: &str, max_width: usize) -> (String, usize) {
    if max_width == 0 || text.is_empty() {
        return (String::new(), 0);
    }
    if is_printable_ascii(text) {
        let clipped: String = text.chars().take(max_width).collect();
        let width = clipped.chars().count();
        return (clipped, width);
    }
    if !text.contains('\u{1b}') && !text.contains('\t') {
        let mut result = String::new();
        let mut width = 0;
        for cluster in grapheme_clusters(text) {
            let w = grapheme_width(&cluster);
            if width + w > max_width {
                break;
            }
            result.push_str(&cluster);
            width += w;
        }
        return (result, width);
    }
    // ANSI-aware path.
    let chars: Vec<char> = text.chars().collect();
    let mut result = String::new();
    let mut width = 0;
    let mut i = 0;
    while i < chars.len() {
        if chars[i] == '\t' {
            if width + 3 > max_width {
                break;
            }
            result.push('\t');
            width += 3;
            i += 1;
            continue;
        }
        let mut end = i;
        while end < chars.len() && chars[end] != '\t' && extract_ansi_code(&chars, end).is_none() {
            end += 1;
        }
        for cluster in grapheme_clusters(&chars[i..end].iter().collect::<String>()) {
            let w = grapheme_width(&cluster);
            if width + w > max_width {
                return (result, width);
            }
            result.push_str(&cluster);
            width += w;
        }
        i = end;
    }
    (result, width)
}

fn finalize_truncated_result(
    prefix: &str,
    prefix_width: usize,
    ellipsis: &str,
    ellipsis_width: usize,
    max_width: usize,
    pad: bool,
) -> String {
    let reset = "\u{1b}[0m";
    let hyperlink_close = get_active_osc8_close(prefix);
    let visible_width_total = prefix_width + ellipsis_width;
    let result = if !ellipsis.is_empty() {
        format!("{prefix}{hyperlink_close}{reset}{ellipsis}{reset}")
    } else {
        format!("{prefix}{hyperlink_close}{reset}")
    };
    if pad {
        format!(
            "{result}{}",
            " ".repeat(max_width.saturating_sub(visible_width_total))
        )
    } else {
        result
    }
}

/// Truncate text to fit a maximum visible width, adding an ellipsis when
/// truncated (upstream `truncateToWidth`). Optionally pads to exactly
/// `max_width`. ANSI escape codes don't count toward width.
pub fn truncate_to_width(text: &str, max_width: usize, ellipsis: &str, pad: bool) -> String {
    if max_width == 0 {
        return String::new();
    }
    if text.is_empty() {
        return if pad {
            " ".repeat(max_width)
        } else {
            String::new()
        };
    }
    let ellipsis_width = visible_width(ellipsis);
    if ellipsis_width >= max_width {
        let text_width = visible_width(text);
        if text_width <= max_width {
            return if pad {
                format!("{text}{}", " ".repeat(max_width - text_width))
            } else {
                text.to_string()
            };
        }
        let (clipped, clipped_width) = truncate_fragment_to_width(ellipsis, max_width);
        if clipped_width == 0 {
            return if pad {
                " ".repeat(max_width)
            } else {
                String::new()
            };
        }
        return finalize_truncated_result("", 0, &clipped, clipped_width, max_width, pad);
    }
    if is_printable_ascii(text) {
        let len = text.chars().count();
        if len <= max_width {
            return if pad {
                format!("{text}{}", " ".repeat(max_width - len))
            } else {
                text.to_string()
            };
        }
        let target_width = max_width - ellipsis_width;
        let prefix: String = text.chars().take(target_width).collect();
        return finalize_truncated_result(
            &prefix,
            target_width,
            ellipsis,
            ellipsis_width,
            max_width,
            pad,
        );
    }

    let target_width = max_width - ellipsis_width;
    let mut result = String::new();
    let mut pending_ansi = String::new();
    let mut visible_so_far = 0usize;
    let mut kept_width = 0usize;
    let mut keep_contiguous_prefix = true;
    let mut overflowed = false;
    let exhausted_input;
    let has_ansi = text.contains('\u{1b}');
    let has_tabs = text.contains('\t');

    if !has_ansi && !has_tabs {
        for cluster in grapheme_clusters(text) {
            let width = grapheme_width(&cluster);
            if keep_contiguous_prefix && kept_width + width <= target_width {
                result.push_str(&cluster);
                kept_width += width;
            } else {
                keep_contiguous_prefix = false;
            }
            visible_so_far += width;
            if visible_so_far > max_width {
                overflowed = true;
                break;
            }
        }
        exhausted_input = !overflowed;
    } else {
        let chars: Vec<char> = text.chars().collect();
        let mut i = 0;
        while i < chars.len() {
            if let Some(ansi) = extract_ansi_code(&chars, i) {
                pending_ansi.push_str(&ansi.code);
                i += ansi.length;
                continue;
            }
            if chars[i] == '\t' {
                if keep_contiguous_prefix && kept_width + 3 <= target_width {
                    if !pending_ansi.is_empty() {
                        result.push_str(&pending_ansi);
                        pending_ansi.clear();
                    }
                    result.push('\t');
                    kept_width += 3;
                } else {
                    keep_contiguous_prefix = false;
                    pending_ansi.clear();
                }
                visible_so_far += 3;
                if visible_so_far > max_width {
                    overflowed = true;
                    break;
                }
                i += 1;
                continue;
            }
            let mut end = i;
            while end < chars.len()
                && chars[end] != '\t'
                && extract_ansi_code(&chars, end).is_none()
            {
                end += 1;
            }
            for cluster in grapheme_clusters(&chars[i..end].iter().collect::<String>()) {
                let width = grapheme_width(&cluster);
                if keep_contiguous_prefix && kept_width + width <= target_width {
                    if !pending_ansi.is_empty() {
                        result.push_str(&pending_ansi);
                        pending_ansi.clear();
                    }
                    result.push_str(&cluster);
                    kept_width += width;
                } else {
                    keep_contiguous_prefix = false;
                    pending_ansi.clear();
                }
                visible_so_far += width;
                if visible_so_far > max_width {
                    overflowed = true;
                    break;
                }
            }
            if overflowed {
                break;
            }
            i = end;
        }
        exhausted_input = i >= chars.len();
    }

    if !overflowed && exhausted_input {
        return if pad {
            format!(
                "{text}{}",
                " ".repeat(max_width.saturating_sub(visible_so_far))
            )
        } else {
            text.to_string()
        };
    }
    finalize_truncated_result(
        &result,
        kept_width,
        ellipsis,
        ellipsis_width,
        max_width,
        pad,
    )
}

/// The terminal-cell range occupied by the grapheme at a visible column
/// (upstream `getGraphemeCellRange`).
pub fn get_grapheme_cell_range(line: &str, column: usize) -> Option<(usize, usize)> {
    let chars: Vec<char> = line.chars().collect();
    let mut current_col = 0usize;
    let mut i = 0usize;
    while i < chars.len() {
        if let Some(ansi) = extract_ansi_code(&chars, i) {
            i += ansi.length;
            continue;
        }
        let mut text_end = i;
        while text_end < chars.len() && extract_ansi_code(&chars, text_end).is_none() {
            text_end += 1;
        }
        let text: String = chars[i..text_end].iter().collect();
        for segment in grapheme_clusters(&text) {
            let width = grapheme_width(&segment);
            if width > 0 && column >= current_col && column < current_col + width {
                return Some((current_col, current_col + width));
            }
            current_col += width;
        }
        i = text_end;
    }
    None
}

/// Parse an OSC 8 hyperlink sequence into its URI (upstream the regex in
/// `getOsc8LinkAtColumn`). An empty URI closes the link.
fn parse_osc8_code(code: &str) -> Option<String> {
    let rest = code.strip_prefix("\u{1b}]8;")?;
    let rest = rest
        .strip_suffix('\u{7}')
        .or_else(|| rest.strip_suffix("\u{1b}\\"))?;
    let uri = rest.split_once(';')?.1;
    if uri.contains('\u{7}') || uri.contains('\u{1b}') {
        return None;
    }
    Some(uri.to_string())
}

/// The OSC 8 hyperlink covering a visible terminal column (upstream
/// `getOsc8LinkAtColumn`).
pub fn get_osc8_link_at_column(line: &str, column: usize) -> Option<String> {
    let chars: Vec<char> = line.chars().collect();
    let mut active_url: Option<String> = None;
    let mut current_col = 0usize;
    let mut i = 0usize;
    while i < chars.len() {
        if let Some(ansi) = extract_ansi_code(&chars, i) {
            if let Some(uri) = parse_osc8_code(&ansi.code) {
                active_url = if uri.is_empty() { None } else { Some(uri) };
            }
            i += ansi.length;
            continue;
        }
        let mut text_end = i;
        while text_end < chars.len() && extract_ansi_code(&chars, text_end).is_none() {
            text_end += 1;
        }
        let text: String = chars[i..text_end].iter().collect();
        for segment in grapheme_clusters(&text) {
            let width = if segment == "\t" {
                3
            } else {
                grapheme_width(&segment)
            };
            if width > 0 && column >= current_col && column < current_col + width {
                return active_url;
            }
            current_col += width;
        }
        i = text_end;
    }
    None
}
