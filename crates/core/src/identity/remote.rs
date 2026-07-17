//! Git remote URL normalization (spec 03 §1.2 `remote_fingerprint`).
//!
//! Different transports for the same repository — `git@host:org/repo.git`,
//! `ssh://git@host/org/repo`, `https://user:pass@host/org/repo.git`,
//! `git://host/org/repo` — normalize to a single canonical string
//! `host/org/repo` (host lowercased; scheme, credentials, port, `.git` suffix
//! and surrounding slashes removed), so SSH and HTTPS remotes for one repo
//! produce the **same** fingerprint. Credentials are always stripped.
//!
//! The resulting fingerprint is a *hint*, nullable and NOT unique
//! (spec 03 §2.1): the registry may map the same remote to more than one
//! repository, so equivalence here never stands in for identity.

use crate::identity::domain;

/// Normalize a git remote URL to its canonical `host/path` form.
///
/// The path's case is preserved (a server may be case-sensitive); only the host
/// is lowercased. Malformed input is normalized best-effort — validation is the
/// registry's concern, not this primitive's.
pub fn normalize_remote_url(url: &str) -> String {
    let trimmed = url.trim();
    let has_scheme = trimmed.contains("://");
    let rest = trimmed.splitn(2, "://").last().unwrap_or(trimmed);

    let (authority, path) = if has_scheme {
        rest.split_once('/').unwrap_or((rest, ""))
    } else {
        // SCP-like `host:path` when a colon precedes any slash; otherwise
        // treat as `host/path`.
        let colon = rest.find(':');
        let slash = rest.find('/');
        match colon {
            Some(c) if slash.is_none_or(|s| c < s) => (&rest[..c], &rest[c + 1..]),
            _ => rest.split_once('/').unwrap_or((rest, "")),
        }
    };

    // Drop userinfo (before the last '@' in the authority) and any port.
    let authority = authority
        .rsplit_once('@')
        .map_or(authority, |(_, host)| host);
    let host = authority
        .split(':')
        .next()
        .unwrap_or(authority)
        .to_ascii_lowercase();

    let path = path.trim_matches('/');
    let path = path.strip_suffix(".git").unwrap_or(path);
    let path = path.trim_end_matches('/');

    if path.is_empty() {
        host
    } else {
        format!("{host}/{path}")
    }
}

/// Fingerprint a raw git remote URL: `H(remote_fingerprint, normalize(url))`
/// (spec 03 §1.2). Convenience over [`normalize_remote_url`] +
/// [`domain::remote_fingerprint`].
pub fn fingerprint(url: &str) -> String {
    domain::remote_fingerprint(&normalize_remote_url(url))
}

#[cfg(test)]
mod tests {
    use super::*;

    const EQUIVALENT: &[&str] = &[
        "git@github.com:org/repo.git",
        "ssh://git@github.com/org/repo.git",
        "ssh://git@github.com:22/org/repo.git",
        "https://github.com/org/repo.git",
        "https://user:pass@github.com/org/repo",
        "https://github.com/org/repo/",
        "git://github.com/org/repo.git",
    ];

    #[test]
    fn transports_for_one_repo_normalize_equally() {
        let canonical = "github.com/org/repo";
        for url in EQUIVALENT {
            assert_eq!(normalize_remote_url(url), canonical, "url = {url}");
        }
    }

    #[test]
    fn transports_for_one_repo_share_a_fingerprint() {
        let first = fingerprint(EQUIVALENT[0]);
        for url in EQUIVALENT {
            assert_eq!(fingerprint(url), first, "url = {url}");
        }
        // And the convenience helper agrees with the explicit composition.
        assert_eq!(
            first,
            domain::remote_fingerprint(&normalize_remote_url(EQUIVALENT[0])),
        );
    }

    #[test]
    fn credentials_are_stripped() {
        let normalized = normalize_remote_url("https://user:s3cr3t@github.com/org/repo.git");
        assert_eq!(normalized, "github.com/org/repo");
        assert!(!normalized.contains("user"));
        assert!(!normalized.contains("s3cr3t"));
    }

    #[test]
    fn host_is_lowercased_path_case_preserved() {
        assert_eq!(
            normalize_remote_url("git@GitHub.COM:Org/Repo.git"),
            "github.com/Org/Repo",
        );
    }

    #[test]
    fn distinct_repos_differ() {
        assert_ne!(
            normalize_remote_url("git@github.com:org/repo.git"),
            normalize_remote_url("git@github.com:org/other.git"),
        );
        assert_ne!(
            fingerprint("git@github.com:org/repo.git"),
            fingerprint("git@github.com:org/other.git")
        );
    }

    #[test]
    fn nested_path_is_preserved() {
        assert_eq!(
            normalize_remote_url("https://gitlab.example.com/group/subgroup/repo.git"),
            "gitlab.example.com/group/subgroup/repo",
        );
    }
}
