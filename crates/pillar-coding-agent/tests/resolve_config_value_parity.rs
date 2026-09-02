//! Port of the upstream resolve-config-value behavior (pi v0.84.3): template
//! parsing, env interpolation, escape sequences, command execution, and the
//! throw-shaped error messages.

use std::collections::BTreeMap;

use pillar_coding_agent::core::resolve_config_value::{
    clear_config_value_cache, get_config_value_env_var_name, get_config_value_env_var_names,
    get_missing_config_value_env_var_names, is_command_config_value, is_config_value_configured,
    resolve_config_value, resolve_config_value_or_throw, resolve_config_value_uncached,
    resolve_headers, resolve_headers_or_throw,
};

fn env(entries: &[(&str, &str)]) -> BTreeMap<String, String> {
    entries
        .iter()
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect()
}

// --- Template parsing --------------------------------------------------------

#[test]
fn resolves_plain_literal() {
    assert_eq!(
        resolve_config_value("just-a-key", None),
        Some("just-a-key".to_string())
    );
}

#[test]
fn interpolates_bare_env_reference() {
    let env = env(&[("MY_KEY", "secret")]);
    assert_eq!(
        resolve_config_value("$MY_KEY", Some(&env)),
        Some("secret".to_string())
    );
}

#[test]
fn interpolates_braced_env_reference() {
    let env = env(&[("MY_KEY", "secret")]);
    assert_eq!(
        resolve_config_value("prefix-${MY_KEY}-suffix", Some(&env)),
        Some("prefix-secret-suffix".to_string())
    );
}

#[test]
fn escapes_dollar_dollar_and_dollar_bang() {
    let env = env(&[("MY_KEY", "secret")]);
    assert_eq!(
        resolve_config_value("$$5", Some(&env)),
        Some("$5".to_string())
    );
    assert_eq!(
        resolve_config_value("$!command", Some(&env)),
        Some("!command".to_string())
    );
}

#[test]
fn lone_dollar_stays_literal() {
    assert_eq!(
        resolve_config_value("cost: 5$", None),
        Some("cost: 5$".to_string())
    );
    assert_eq!(
        resolve_config_value("${not a var}", None),
        Some("${not a var}".to_string())
    );
}

#[test]
fn missing_env_reference_resolves_to_none() {
    assert_eq!(resolve_config_value("$MISSING_VAR_XYZ", None), None);
}

#[test]
fn env_scoped_overrides_process_env() {
    // The scoped env wins; when it lacks the var, process env is consulted.
    let scoped = env(&[("MY_KEY", "scoped")]);
    assert_eq!(
        resolve_config_value("$MY_KEY", Some(&scoped)),
        Some("scoped".to_string())
    );
}

#[test]
fn empty_env_value_counts_as_unset() {
    let env = env(&[("MY_KEY", "")]);
    assert_eq!(resolve_config_value("$MY_KEY", Some(&env)), None);
}

// --- env var name helpers -----------------------------------------------------

#[test]
fn single_env_reference_reports_its_name() {
    assert_eq!(
        get_config_value_env_var_name("$MY_KEY"),
        Some("MY_KEY".to_string())
    );
    assert_eq!(
        get_config_value_env_var_name("${MY_KEY}"),
        Some("MY_KEY".to_string())
    );
    assert_eq!(get_config_value_env_var_name("prefix-$MY_KEY"), None);
    assert_eq!(get_config_value_env_var_name("!command"), None);
}

#[test]
fn lists_all_referenced_env_var_names_deduplicated() {
    assert_eq!(
        get_config_value_env_var_names("$A-$B-$A-${C}"),
        vec!["A".to_string(), "B".to_string(), "C".to_string()]
    );
}

#[test]
fn reports_missing_env_var_names() {
    let env = env(&[("PRESENT", "1")]);
    assert_eq!(
        get_missing_config_value_env_var_names("$PRESENT-$MISSING", Some(&env)),
        vec!["MISSING".to_string()]
    );
}

// --- configured / command checks ---------------------------------------------

#[test]
fn is_config_value_configured_checks_references() {
    let env = env(&[("A", "1")]);
    assert!(is_config_value_configured("$A", Some(&env)));
    assert!(!is_config_value_configured("$MISSING", Some(&env)));
    assert!(is_config_value_configured("literal", Some(&env)));
}

#[test]
fn detects_command_config_values() {
    assert!(is_command_config_value("!echo hello"));
    assert!(!is_command_config_value("$ESCAPED"));
    assert!(!is_command_config_value("literal"));
}

// --- command execution --------------------------------------------------------

#[test]
fn executes_shell_command_and_trims_stdout() {
    clear_config_value_cache();
    assert_eq!(
        resolve_config_value("!echo hello", None),
        Some("hello".to_string())
    );
}

#[test]
fn failing_command_resolves_to_none() {
    clear_config_value_cache();
    assert_eq!(resolve_config_value("!exit 3", None), None);
}

#[test]
fn command_result_cache_is_used() {
    clear_config_value_cache();
    let first = resolve_config_value("!echo cached-value", None);
    let second = resolve_config_value("!echo cached-value", None);
    assert_eq!(first, second);
    clear_config_value_cache();
}

#[test]
fn uncached_resolution_skips_the_cache() {
    clear_config_value_cache();
    assert_eq!(
        resolve_config_value_uncached("!echo uncached", None),
        Some("uncached".to_string())
    );
}

// --- or-throw errors ----------------------------------------------------------

#[test]
fn or_throw_reports_missing_command() {
    let error = resolve_config_value_or_throw("!exit 3", "API key", None).expect_err("must fail");
    assert!(
        error.contains("Failed to resolve API key from shell command: exit 3"),
        "unexpected: {error}"
    );
}

#[test]
fn or_throw_reports_single_missing_env_var() {
    let error =
        resolve_config_value_or_throw("$MISSING_ONE", "API key", None).expect_err("must fail");
    assert!(
        error.contains("Failed to resolve API key from environment variable: MISSING_ONE"),
        "unexpected: {error}"
    );
}

#[test]
fn or_throw_reports_multiple_missing_env_vars() {
    let error = resolve_config_value_or_throw("$MISSING_A-$MISSING_B", "API key", None)
        .expect_err("must fail");
    assert!(
        error.contains("environment variables: MISSING_A, MISSING_B"),
        "unexpected: {error}"
    );
}

#[test]
fn or_throw_returns_value_when_resolvable() {
    let env = env(&[("OK_VAR", "value")]);
    assert_eq!(
        resolve_config_value_or_throw("$OK_VAR", "API key", Some(&env)).expect("resolves"),
        "value"
    );
}

// --- headers -------------------------------------------------------------------

#[test]
fn resolves_all_header_values() {
    let env = env(&[("TOKEN", "t0k3n")]);
    let headers = BTreeMap::from([
        ("Authorization".to_string(), "Bearer $TOKEN".to_string()),
        ("X-Static".to_string(), "static".to_string()),
        ("X-Dropped".to_string(), "$MISSING".to_string()),
    ]);
    let resolved = resolve_headers(Some(&headers), Some(&env)).expect("resolved");
    assert_eq!(
        resolved.get("Authorization").map(String::as_str),
        Some("Bearer t0k3n")
    );
    assert_eq!(resolved.get("X-Static").map(String::as_str), Some("static"));
    assert!(
        !resolved.contains_key("X-Dropped"),
        "unresolvable headers are dropped"
    );
}

#[test]
fn resolve_headers_none_for_empty_result() {
    let headers = BTreeMap::from([("X-Dropped".to_string(), "$MISSING".to_string())]);
    assert_eq!(resolve_headers(Some(&headers), None), None);
    assert_eq!(resolve_headers(None, None), None);
}

#[test]
fn resolve_headers_or_throw_fails_on_unresolvable() {
    let headers = BTreeMap::from([("Authorization".to_string(), "$MISSING".to_string())]);
    let error = resolve_headers_or_throw(Some(&headers), "provider", None).expect_err("must fail");
    assert!(
        error.contains("provider header \"Authorization\""),
        "unexpected: {error}"
    );
}
