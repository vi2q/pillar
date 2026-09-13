//! Port of packages/coding-agent/src/utils/git.ts (pi v0.84.3): git source
//! parsing.
//!
//! divergence: upstream delegates host recognition to the `hosted-git-info`
//! package (gist, sr.ht, bitbucket URL munging). The port keeps the generic
//! parser: scp-like, protocol URLs, and `host/owner/repo[@ref]` shorthands
//! are recognised, while host-specific rewrite rules are not.

/// A parsed git source (upstream `GitSource`; `type` is always "git" and
/// implicit in the Rust type).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GitSource {
    /// Clone URL (always valid for git clone, without ref suffix).
    pub repo: String,
    /// Git host domain (e.g., "github.com").
    pub host: String,
    /// Repository path (e.g., "user/repo").
    pub path: String,
    /// Git ref (branch, tag, commit) if specified.
    pub ref_: Option<String>,
    /// True if ref was specified (package won't be auto-updated).
    pub pinned: bool,
}

struct SplitRef {
    repo: String,
    ref_: Option<String>,
}

fn split_ref(url: &str) -> SplitRef {
    // scp-like: git@host:path[@ref]
    if let Some(rest) = url.strip_prefix("git@") {
        if let Some(colon) = rest.find(':') {
            let host = &rest[..colon];
            let path_with_maybe_ref = &rest[colon + 1..];
            if let Some(at) = path_with_maybe_ref.find('@') {
                let repo_path = &path_with_maybe_ref[..at];
                let ref_ = &path_with_maybe_ref[at + 1..];
                if !repo_path.is_empty() && !ref_.is_empty() {
                    return SplitRef {
                        repo: format!("git@{host}:{repo_path}"),
                        ref_: Some(ref_.to_string()),
                    };
                }
            }
            return SplitRef {
                repo: url.to_string(),
                ref_: None,
            };
        }
    }

    if url.contains("://") {
        if let Some(scheme_end) = url.find("://") {
            let after = &url[scheme_end + 3..];
            // Strip query/fragment-free path portion.
            let path_start = after.find('/').map(|i| i + scheme_end + 3);
            if let Some(ps) = path_start {
                let path_with_maybe_ref = &url[ps..];
                let path_with_maybe_ref = path_with_maybe_ref.trim_start_matches('/');
                if let Some(at) = path_with_maybe_ref.find('@') {
                    let repo_path = &path_with_maybe_ref[..at];
                    let ref_ = &path_with_maybe_ref[at + 1..];
                    if !repo_path.is_empty() && !ref_.is_empty() {
                        let base = &url[..ps + 1];
                        return SplitRef {
                            repo: format!("{base}{repo_path}"),
                            ref_: Some(ref_.to_string()),
                        };
                    }
                }
            }
            return SplitRef {
                repo: url.to_string(),
                ref_: None,
            };
        }
    }

    // Shorthand host/path[@ref]
    let Some(slash) = url.find('/') else {
        return SplitRef {
            repo: url.to_string(),
            ref_: None,
        };
    };
    let host = &url[..slash];
    let path_with_maybe_ref = &url[slash + 1..];
    if let Some(at) = path_with_maybe_ref.find('@') {
        let repo_path = &path_with_maybe_ref[..at];
        let ref_ = &path_with_maybe_ref[at + 1..];
        if !repo_path.is_empty() && !ref_.is_empty() {
            return SplitRef {
                repo: format!("{host}/{repo_path}"),
                ref_: Some(ref_.to_string()),
            };
        }
    }
    SplitRef {
        repo: url.to_string(),
        ref_: None,
    }
}

fn decode_for_validation(value: &str) -> Option<String> {
    percent_decode(value)
}

/// Minimal percent-decoder for validation purposes.
fn percent_decode(value: &str) -> Option<String> {
    let bytes = value.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' {
            if i + 2 >= bytes.len() {
                return None;
            }
            let hex = std::str::from_utf8(&bytes[i + 1..i + 3]).ok()?;
            let byte = u8::from_str_radix(hex, 16).ok()?;
            out.push(byte);
            i += 3;
        } else {
            out.push(bytes[i]);
            i += 1;
        }
    }
    String::from_utf8(out).ok()
}

fn has_unsafe_git_install_part(value: &str, allow_slash: bool) -> bool {
    let Some(decoded) = decode_for_validation(value) else {
        return true;
    };
    for candidate in [value.to_string(), decoded] {
        if candidate.contains('\0') || candidate.contains('\\') || candidate.starts_with('/') {
            return true;
        }
        if !allow_slash && candidate.contains('/') {
            return true;
        }
        if candidate.split('/').any(|segment| segment == "..") {
            return true;
        }
    }
    false
}

fn build_git_source(repo: &str, host: &str, path: &str, ref_: Option<&str>) -> Option<GitSource> {
    if path.starts_with('/') {
        return None;
    }
    let normalized_path = path.trim_end_matches(".git").trim_start_matches('/');
    let normalized_path = normalized_path.trim_end_matches(".git");
    if host.is_empty() || normalized_path.is_empty() || normalized_path.split('/').count() < 2 {
        return None;
    }
    if has_unsafe_git_install_part(host, false)
        || has_unsafe_git_install_part(normalized_path, true)
    {
        return None;
    }
    Some(GitSource {
        repo: repo.to_string(),
        host: host.to_string(),
        path: normalized_path.to_string(),
        ref_: ref_.map(str::to_string),
        pinned: ref_.is_some(),
    })
}

fn parse_generic_git_url(url: &str) -> Option<GitSource> {
    let split = split_ref(url);
    let repo_without_ref = &split.repo;
    let mut repo = repo_without_ref.clone();

    let (host, path) = if let Some(rest) = repo_without_ref.strip_prefix("git@") {
        let colon = rest.find(':')?;
        (&rest[..colon], &rest[colon + 1..])
    } else if repo_without_ref.starts_with("https://")
        || repo_without_ref.starts_with("http://")
        || repo_without_ref.starts_with("ssh://")
        || repo_without_ref.starts_with("git://")
    {
        let after = &repo_without_ref[repo_without_ref.find("://").map(|i| i + 3).unwrap_or(0)..];
        let slash = after.find('/')?;
        // Strip the userinfo (user@) from the host, like URL.hostname.
        let authority = &after[..slash];
        let authority = authority.rsplit('@').next().unwrap_or(authority);
        (authority, after[slash + 1..].trim_start_matches('/'))
    } else {
        let slash = repo_without_ref.find('/')?;
        let host = &repo_without_ref[..slash];
        let path = &repo_without_ref[slash + 1..];
        if !host.contains('.') && host != "localhost" {
            return None;
        }
        repo = format!("https://{repo_without_ref}");
        (host, path)
    };

    build_git_source(&repo, host, path, split.ref_.as_deref())
}

/// Parse a git source (upstream `parseGitUrl`). Rules: with a `git:`
/// prefix, accept all historical shorthand forms; without it, only accept
/// explicit protocol URLs. hosted-git-info specifics (gist, other hosts'
/// URL munging) are not ported — the generic parser handles
/// `host/owner/repo` shorthands.
pub fn parse_git_url(source: &str) -> Option<GitSource> {
    let trimmed = source.trim();
    let has_git_prefix = trimmed.starts_with("git:");
    let url = if has_git_prefix {
        trimmed[4..].trim()
    } else {
        trimmed
    };

    let protocol_re = |u: &str| {
        u.starts_with("https://")
            || u.starts_with("http://")
            || u.starts_with("ssh://")
            || u.starts_with("git://")
    };
    if !has_git_prefix && !protocol_re(url) {
        return None;
    }

    parse_generic_git_url(url)
}
