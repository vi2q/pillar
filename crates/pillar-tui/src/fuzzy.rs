//! Port of packages/tui/src/fuzzy.ts (pi v0.84.3): subsequence fuzzy
//! matching with word-boundary and consecutive-run bonuses, swapped
//! alpha/digit retry, and tokenized filtering/sorting.

/// A fuzzy match result (upstream `FuzzyMatch`); lower score is better.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct FuzzyMatch {
    pub matches: bool,
    pub score: f64,
}

const WORD_BOUNDARY_CHARS: [char; 6] = [' ', '-', '_', '.', '/', ':'];

fn is_word_boundary(previous: char) -> bool {
    WORD_BOUNDARY_CHARS.contains(&previous)
}

/// Check whether a swapped alpha/digit form of the query exists (upstream
/// the two regexes): "abc123" ↔ "123abc".
fn swapped_query(query_lower: &str) -> Option<String> {
    let bytes = query_lower.as_bytes();
    let is_alpha = |b: u8| b.is_ascii_lowercase();
    let is_digit = |b: u8| b.is_ascii_digit();

    // ^([a-z]+)([0-9]+)$
    if bytes.len() >= 2 {
        let alpha = bytes.iter().take_while(|b| is_alpha(**b)).count();
        if alpha > 0 && alpha < bytes.len() && bytes[alpha..].iter().all(|b| is_digit(*b)) {
            return Some(format!(
                "{}{}",
                &query_lower[alpha..],
                &query_lower[..alpha]
            ));
        }
        // ^([0-9]+)([a-z]+)$
        let digits = bytes.iter().take_while(|b| is_digit(**b)).count();
        if digits > 0 && digits < bytes.len() && bytes[digits..].iter().all(|b| is_alpha(*b)) {
            return Some(format!(
                "{}{}",
                &query_lower[digits..],
                &query_lower[..digits]
            ));
        }
    }
    None
}

fn match_query(normalized_query: &str, text_lower: &str) -> FuzzyMatch {
    if normalized_query.is_empty() {
        return FuzzyMatch {
            matches: true,
            score: 0.0,
        };
    }
    if normalized_query.chars().count() > text_lower.chars().count() {
        return FuzzyMatch {
            matches: false,
            score: 0.0,
        };
    }

    let query_chars: Vec<char> = normalized_query.chars().collect();
    let text_chars: Vec<char> = text_lower.chars().collect();
    let mut query_index = 0usize;
    let mut score = 0.0f64;
    let mut last_match_index: isize = -1;
    let mut consecutive_matches = 0usize;

    for (i, ch) in text_chars.iter().enumerate() {
        if query_index >= query_chars.len() {
            break;
        }
        if *ch == query_chars[query_index] {
            let is_boundary = i == 0 || is_word_boundary(text_chars[i - 1]);

            // Reward consecutive matches.
            if last_match_index == i as isize - 1 {
                consecutive_matches += 1;
                score -= (consecutive_matches * 5) as f64;
            } else {
                consecutive_matches = 0;
                // Penalize gaps.
                if last_match_index >= 0 {
                    score += ((i as isize - last_match_index - 1) * 2) as f64;
                }
            }

            // Reward word boundary matches.
            if is_boundary {
                score -= 10.0;
            }

            // Slight penalty for later matches.
            score += i as f64 * 0.1;

            last_match_index = i as isize;
            query_index += 1;
        }
    }

    if query_index < query_chars.len() {
        return FuzzyMatch {
            matches: false,
            score: 0.0,
        };
    }

    if normalized_query == text_lower {
        score -= 100.0;
    }

    FuzzyMatch {
        matches: true,
        score,
    }
}

/// Subsequence fuzzy match (upstream `fuzzyMatch`): all query characters
/// must appear in order; score rewards consecutive/word-boundary matches
/// and penalizes gaps and later positions. A swapped alpha/digit form
/// (e.g. "abc123" vs "123abc") is retried with a +5 penalty.
pub fn fuzzy_match(query: &str, text: &str) -> FuzzyMatch {
    let query_lower = query.to_lowercase();
    let text_lower = text.to_lowercase();

    let primary = match_query(&query_lower, &text_lower);
    if primary.matches {
        return primary;
    }

    let Some(swapped) = swapped_query(&query_lower) else {
        return primary;
    };
    let swapped_match = match_query(&swapped, &text_lower);
    if !swapped_match.matches {
        return primary;
    }
    FuzzyMatch {
        matches: true,
        score: swapped_match.score + 5.0,
    }
}

/// Filter and sort items by fuzzy match quality, best first (upstream
/// `fuzzyFilter`): whitespace- and slash-separated tokens must all match;
/// token scores sum.
pub fn fuzzy_filter<'a, I, F>(items: I, query: &str, get_text: F) -> Vec<&'a str>
where
    I: IntoIterator<Item = &'a str>,
    F: Fn(&str) -> String,
{
    let _ = &get_text;
    if query.trim().is_empty() {
        return items.into_iter().collect();
    }
    let tokens: Vec<&str> = query
        .trim()
        .split(|c: char| c.is_whitespace() || c == '/')
        .filter(|t| !t.is_empty())
        .collect();
    if tokens.is_empty() {
        return items.into_iter().collect();
    }

    let mut results: Vec<(&'a str, f64)> = Vec::new();
    for item in items {
        let text = item.to_lowercase();
        let mut total_score = 0.0f64;
        let mut all_match = true;
        for token in &tokens {
            let token_lower = token.to_lowercase();
            let matched = fuzzy_match(token, &text);
            if matched.matches {
                total_score += matched.score;
            } else {
                // Fall back to an exact case-insensitive subsequence check
                // against the lowercased text for token scoring parity.
                if text.contains(&token_lower) {
                    // Matched after lowercase folding inside fuzzy_match;
                    // this branch is unreachable in practice.
                    total_score += 0.0;
                } else {
                    all_match = false;
                    break;
                }
            }
        }
        if all_match {
            results.push((item, total_score));
        }
    }
    results.sort_by(|a, b| a.1.partial_cmp(&b.1).unwrap_or(std::cmp::Ordering::Equal));
    results.into_iter().map(|(item, _)| item).collect()
}

/// Generic fuzzy filter over any item type (upstream `fuzzyFilter<T>`).
pub fn fuzzy_filter_by<'a, T, F>(items: &'a [T], query: &str, get_text: F) -> Vec<&'a T>
where
    F: Fn(&T) -> String,
{
    if query.trim().is_empty() {
        return items.iter().collect();
    }
    let tokens: Vec<String> = query
        .trim()
        .split(|c: char| c.is_whitespace() || c == '/')
        .filter(|t| !t.is_empty())
        .map(str::to_lowercase)
        .collect();
    if tokens.is_empty() {
        return items.iter().collect();
    }

    let mut results: Vec<(&T, f64)> = Vec::new();
    for item in items {
        let text = get_text(item).to_lowercase();
        let mut total_score = 0.0f64;
        let mut all_match = true;
        for token in &tokens {
            let matched = fuzzy_match(token, &text);
            if matched.matches {
                total_score += matched.score;
            } else {
                all_match = false;
                break;
            }
        }
        if all_match {
            results.push((item, total_score));
        }
    }
    results.sort_by(|a, b| a.1.partial_cmp(&b.1).unwrap_or(std::cmp::Ordering::Equal));
    results.into_iter().map(|(item, _)| item).collect()
}
