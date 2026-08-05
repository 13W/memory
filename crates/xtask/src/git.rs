//! Small helper shared across `bench`, `memory_bench`, and `release_report`
//! (T17-05) — see `crate::stats`'s own doc comment for why a third copy
//! inside this one crate is deduplicated rather than repeated again.

/// The short commit hash of `dir`'s HEAD, or `None` if `dir` is not a git
/// checkout (or `git` is unavailable) — a provenance field, not something
/// worth failing a report over.
pub fn git_short_head(dir: &std::path::Path) -> Option<String> {
    let out = std::process::Command::new("git")
        .args([
            "-C",
            &dir.display().to_string(),
            "rev-parse",
            "--short",
            "HEAD",
        ])
        .output()
        .ok()?;
    out.status
        .success()
        .then(|| String::from_utf8_lossy(&out.stdout).trim().to_string())
}
