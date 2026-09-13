//! Parity tests for utils/git.ts (pi v0.84.3, upstream
//! test/git-ssh-url.test.ts): accepted protocol URLs, git:-prefixed
//! shorthands, unsafe-input rejection, and the forms that must stay
//! unsupported without the prefix.

use pillar_coding_agent::utils::git::parse_git_url;

#[test]
fn parses_protocol_urls_without_the_git_prefix() {
    let https = parse_git_url("https://github.com/user/repo").expect("https url");
    assert_eq!(https.host, "github.com");
    assert_eq!(https.path, "user/repo");
    assert_eq!(https.repo, "https://github.com/user/repo");
    assert_eq!(https.ref_, None);
    assert!(!https.pinned);

    let ssh = parse_git_url("ssh://git@github.com/user/repo").expect("ssh url");
    assert_eq!(ssh.host, "github.com");
    assert_eq!(ssh.path, "user/repo");
    assert_eq!(ssh.repo, "ssh://git@github.com/user/repo");

    let with_ref = parse_git_url("https://github.com/user/repo@v1.0.0").expect("ref url");
    assert_eq!(with_ref.host, "github.com");
    assert_eq!(with_ref.path, "user/repo");
    assert_eq!(with_ref.ref_.as_deref(), Some("v1.0.0"));
    assert_eq!(with_ref.repo, "https://github.com/user/repo");
    assert!(with_ref.pinned);
}

#[test]
fn parses_shorthands_with_the_git_prefix() {
    let scp_like = parse_git_url("git:git@github.com:user/repo").expect("scp-like");
    assert_eq!(scp_like.host, "github.com");
    assert_eq!(scp_like.path, "user/repo");
    assert_eq!(scp_like.repo, "git@github.com:user/repo");

    let shorthand = parse_git_url("git:github.com/user/repo").expect("shorthand");
    assert_eq!(shorthand.host, "github.com");
    assert_eq!(shorthand.path, "user/repo");
    assert_eq!(shorthand.repo, "https://github.com/user/repo");

    let with_ref = parse_git_url("git:git@github.com:user/repo@v1.0.0").expect("scp-like + ref");
    assert_eq!(with_ref.host, "github.com");
    assert_eq!(with_ref.path, "user/repo");
    assert_eq!(with_ref.ref_.as_deref(), Some("v1.0.0"));
    assert_eq!(with_ref.repo, "git@github.com:user/repo");
}

#[test]
fn rejects_unsafe_git_install_paths() {
    for source in [
        "git:git@evil.example:../../victim/repo",
        "https://evil.example/..%2F..%2Fvictim/repo",
        "https://evil.example/..%2F..%2Fvictim/repo%",
        "git:git@evil.example:/absolute/repo",
        "git:git@evil.example:user\\repo/name",
        "git:git@evil.example:user/repo\0name",
    ] {
        assert!(parse_git_url(source).is_none(), "must reject {source}");
    }
}

#[test]
fn rejects_shorthand_forms_without_the_git_prefix() {
    assert!(parse_git_url("git@github.com:user/repo").is_none());
    assert!(parse_git_url("github.com/user/repo").is_none());
    assert!(parse_git_url("user/repo").is_none());
}
