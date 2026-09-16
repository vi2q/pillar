//! Port of packages/coding-agent/src/modes/interactive/components/
//! session-selector-search.ts (pi v0.84.3): query parsing, matching, and
//! sorting for the resume-session selector.

use pillar_tui::fuzzy::fuzzy_match;

use crate::core::session_manager::SessionInfo;

/// Upstream `SortMode`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SortMode {
    /// Session tree by parent, most recent subtree first.
    Threaded,
    /// Newest first (list order only; with a query, filter only).
    Recent,
    /// Query score, tie-broken by modified date.
    Relevance,
}

/// Upstream `NameFilter`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NameFilter {
    All,
    Named,
}

/// Upstream `ParsedSearchQuery`.
#[derive(Debug, Clone)]
pub struct ParsedSearchQuery {
    pub mode: QueryMode,
    pub tokens: Vec<SearchToken>,
    pub regex: Option<regex::Regex>,
    /// If set, parsing failed and the query should match nothing.
    pub error: Option<String>,
}

/// Upstream the "tokens" | "regex" mode union.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum QueryMode {
    Tokens,
    Regex,
}

/// Upstream `{ kind: "fuzzy" | "phrase"; value: string }`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SearchToken {
    Fuzzy(String),
    Phrase(String),
}

/// Lower is better; only meaningful when `matches` is true (upstream
/// `MatchResult`).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct MatchResult {
    pub matches: bool,
    pub score: f64,
}

/// Upstream `normalizeWhitespaceLower`.
fn normalize_whitespace_lower(text: &str) -> String {
    text.to_lowercase()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

/// Upstream `getSessionSearchText`.
fn session_search_text(session: &SessionInfo) -> String {
    format!(
        "{} {} {} {}",
        session.id,
        session.name.as_deref().unwrap_or(""),
        session.all_messages_text,
        session.cwd
    )
}

/// Upstream `hasSessionName`.
pub fn has_session_name(session: &SessionInfo) -> bool {
    session
        .name
        .as_deref()
        .is_some_and(|name| !name.trim().is_empty())
}

/// Upstream `parseSearchQuery`: `re:<pattern>` regex mode, else whitespace
/// tokens with `"phrase"` support (unbalanced quotes fall back to plain
/// tokenization).
pub fn parse_search_query(query: &str) -> ParsedSearchQuery {
    let trimmed = query.trim();
    if trimmed.is_empty() {
        return ParsedSearchQuery {
            mode: QueryMode::Tokens,
            tokens: Vec::new(),
            regex: None,
            error: None,
        };
    }

    // Regex mode: re:<pattern>
    if let Some(pattern) = trimmed.strip_prefix("re:") {
        let pattern = pattern.trim();
        if pattern.is_empty() {
            return ParsedSearchQuery {
                mode: QueryMode::Regex,
                tokens: Vec::new(),
                regex: None,
                error: Some("Empty regex".to_string()),
            };
        }
        return match regex::RegexBuilder::new(pattern)
            .case_insensitive(true)
            .build()
        {
            Ok(regex) => ParsedSearchQuery {
                mode: QueryMode::Regex,
                tokens: Vec::new(),
                regex: Some(regex),
                error: None,
            },
            Err(err) => ParsedSearchQuery {
                mode: QueryMode::Regex,
                tokens: Vec::new(),
                regex: None,
                error: Some(err.to_string()),
            },
        };
    }

    // Token mode with quote support.
    // Example: foo "node cve" bar
    let mut tokens: Vec<SearchToken> = Vec::new();
    let mut buf = String::new();
    let mut in_quote = false;
    let mut had_unclosed_quote = false;

    let mut flush = |kind: fn(String) -> SearchToken, buf: &mut String| {
        let value = buf.trim().to_string();
        buf.clear();
        if value.is_empty() {
            return;
        }
        tokens.push(kind(value));
    };

    for ch in trimmed.chars() {
        if ch == '"' {
            if in_quote {
                flush(SearchToken::Phrase, &mut buf);
                in_quote = false;
            } else {
                flush(SearchToken::Fuzzy, &mut buf);
                in_quote = true;
            }
            continue;
        }
        if !in_quote && ch.is_whitespace() {
            flush(SearchToken::Fuzzy, &mut buf);
            continue;
        }
        buf.push(ch);
    }

    if in_quote {
        had_unclosed_quote = true;
    }

    // If quotes were unbalanced, fall back to plain whitespace tokenization.
    if had_unclosed_quote {
        return ParsedSearchQuery {
            mode: QueryMode::Tokens,
            tokens: trimmed
                .split_whitespace()
                .map(str::trim)
                .filter(|token| !token.is_empty())
                .map(|token| SearchToken::Fuzzy(token.to_string()))
                .collect(),
            regex: None,
            error: None,
        };
    }

    flush(SearchToken::Fuzzy, &mut buf);

    ParsedSearchQuery {
        mode: QueryMode::Tokens,
        tokens,
        regex: None,
        error: None,
    }
}

/// Upstream `matchSession`.
pub fn match_session(session: &SessionInfo, parsed: &ParsedSearchQuery) -> MatchResult {
    let text = session_search_text(session);

    if parsed.mode == QueryMode::Regex {
        let Some(regex) = &parsed.regex else {
            return MatchResult {
                matches: false,
                score: 0.0,
            };
        };
        let Some(index) = regex.find(&text) else {
            return MatchResult {
                matches: false,
                score: 0.0,
            };
        };
        return MatchResult {
            matches: true,
            score: index.start() as f64 * 0.1,
        };
    }

    if parsed.tokens.is_empty() {
        return MatchResult {
            matches: true,
            score: 0.0,
        };
    }

    let mut total_score = 0.0;
    let mut normalized_text: Option<String> = None;

    for token in &parsed.tokens {
        match token {
            SearchToken::Phrase(value) => {
                if normalized_text.is_none() {
                    normalized_text = Some(normalize_whitespace_lower(&text));
                }
                let phrase = normalize_whitespace_lower(value);
                if phrase.is_empty() {
                    continue;
                }
                let Some(index) = normalized_text.as_ref().expect("just set").find(&phrase) else {
                    return MatchResult {
                        matches: false,
                        score: 0.0,
                    };
                };
                total_score += index as f64 * 0.1;
            }
            SearchToken::Fuzzy(value) => {
                let matched = fuzzy_match(value, &text);
                if !matched.matches {
                    return MatchResult {
                        matches: false,
                        score: 0.0,
                    };
                }
                total_score += matched.score;
            }
        }
    }

    MatchResult {
        matches: true,
        score: total_score,
    }
}

/// Upstream `filterAndSortSessions`: name-filter first, then the query; in
/// `recent` mode the filtered sessions keep their incoming order, in the
/// other modes they sort by score (modified desc as tie-break).
pub fn filter_and_sort_sessions(
    sessions: &[SessionInfo],
    query: &str,
    sort_mode: SortMode,
    name_filter: NameFilter,
) -> Vec<SessionInfo> {
    let name_filtered: Vec<&SessionInfo> = match name_filter {
        NameFilter::All => sessions.iter().collect(),
        NameFilter::Named => sessions.iter().filter(|s| has_session_name(s)).collect(),
    };
    if query.trim().is_empty() {
        return name_filtered.into_iter().cloned().collect();
    }

    let parsed = parse_search_query(query);
    if parsed.error.is_some() {
        return Vec::new();
    }

    // Recent mode: filter only, keep incoming order. The other modes sort by
    // score with a modified-desc tie-break (upstream only `recent` keeps the
    // incoming order; `threaded` falls into the same scored branch).
    let mut filtered: Vec<(&SessionInfo, f64)> = name_filtered
        .into_iter()
        .filter_map(|session| {
            let result = match_session(session, &parsed);
            result.matches.then_some((session, result.score))
        })
        .collect();
    if sort_mode != SortMode::Recent {
        filtered.sort_by(|a, b| {
            a.1.total_cmp(&b.1)
                .then_with(|| b.0.modified_ms.cmp(&a.0.modified_ms))
        });
    }
    filtered
        .into_iter()
        .map(|(session, _)| session.clone())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn session(
        path: &str,
        id: &str,
        name: Option<&str>,
        modified_ms: u64,
        body: &str,
    ) -> SessionInfo {
        SessionInfo {
            path: path.to_string(),
            id: id.to_string(),
            cwd: "/tmp".to_string(),
            name: name.map(str::to_string),
            parent_session_path: None,
            created_ms: Some(modified_ms),
            modified_ms,
            message_count: 2,
            first_message: body.to_string(),
            all_messages_text: body.to_string(),
        }
    }

    #[test]
    fn parses_the_query_shapes() {
        let parsed = parse_search_query("");
        assert!(parsed.tokens.is_empty() && parsed.error.is_none());

        // re: regex mode (case-insensitive like the upstream `i` flag).
        let parsed = parse_search_query("re:fix CRASH");
        assert_eq!(parsed.mode, QueryMode::Regex);
        assert!(parsed.regex.is_some());
        assert!(parsed.error.is_none());

        let parsed = parse_search_query("re:(");
        assert!(parsed.error.is_some(), "invalid regex reports an error");

        // Quoted phrases survive; unbalanced quotes fall back to plain tokens.
        let parsed = parse_search_query("foo \"node cve\" bar");
        assert_eq!(
            parsed.tokens,
            vec![
                SearchToken::Fuzzy("foo".to_string()),
                SearchToken::Phrase("node cve".to_string()),
                SearchToken::Fuzzy("bar".to_string()),
            ]
        );
        let parsed = parse_search_query("unbalanced \"quote");
        assert_eq!(
            parsed.tokens,
            vec![
                SearchToken::Fuzzy("unbalanced".to_string()),
                SearchToken::Fuzzy("\"quote".to_string()),
            ]
        );
    }

    #[test]
    fn matching_uses_the_session_search_text() {
        let session = session("a.jsonl", "id-1", Some("deploy fix"), 1000, "fixed the bug");
        let parsed = parse_search_query("bug");
        assert!(match_session(&session, &parsed).matches);

        // Phrases match whole sequences; regex anchors on the text position.
        let parsed = parse_search_query("\"the bug\"");
        assert!(match_session(&session, &parsed).matches);
        let parsed = parse_search_query("re:FIXED");
        assert!(match_session(&session, &parsed).matches, "case-insensitive");

        let parsed = parse_search_query("zzzz");
        assert!(!match_session(&session, &parsed).matches);
    }

    #[test]
    fn filter_and_sort_scopes_by_name_and_sort_mode() {
        let sessions = vec![
            session("a.jsonl", "a", None, 1000, "older unnamed"),
            session("b.jsonl", "b", Some("named"), 3000, "newer named bug"),
            session("c.jsonl", "c", None, 2000, "middle unnamed bug"),
        ];

        // No query: the name filter only.
        let named = filter_and_sort_sessions(&sessions, "", SortMode::Recent, NameFilter::Named);
        assert_eq!(
            named.iter().map(|s| s.id.as_str()).collect::<Vec<_>>(),
            vec!["b"]
        );

        // Recent mode with a query: incoming (modified-desc) order is kept.
        let recent = filter_and_sort_sessions(&sessions, "bug", SortMode::Recent, NameFilter::All);
        assert_eq!(
            recent.iter().map(|s| s.id.as_str()).collect::<Vec<_>>(),
            vec!["b", "c"]
        );

        // Relevance mode: score order, modified desc as tie-break (the exact
        // rank follows the upstream scoring — both match, no order assertion).
        let relevance =
            filter_and_sort_sessions(&sessions, "bug", SortMode::Relevance, NameFilter::All);
        assert_eq!(
            {
                let mut ids = relevance.iter().map(|s| s.id.as_str()).collect::<Vec<_>>();
                ids.sort();
                ids
            },
            vec!["b", "c"]
        );

        // An invalid regex matches nothing.
        assert!(
            filter_and_sort_sessions(&sessions, "re:[", SortMode::Recent, NameFilter::All)
                .is_empty()
        );
    }

    #[test]
    fn has_session_name_checks_the_trimmed_name() {
        assert!(has_session_name(&session("a", "a", Some(" name "), 0, "x")));
        assert!(!has_session_name(&session("a", "a", Some("   "), 0, "x")));
        assert!(!has_session_name(&session("a", "a", None, 0, "x")));
    }
}
