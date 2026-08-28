//! Embeds a build identifier (D-050) that changes on every commit — unlike
//! [`CARGO_PKG_VERSION`](std::env!), which this workspace bumps only when a
//! release is cut (the tag *is* the version), never per commit. The literal
//! is deliberately not repeated here: it moved once already, 0.0.0 -> 0.1.0.
//! `local_rag_core::BUILD_ID` needs exactly this property to fingerprint
//! "the code that produced this deterministic failure" (see
//! `local_rag_store::memory::consolidation`'s own doc for the consumer).
//!
//! `git describe --always --dirty` never touches the network and never fails
//! the build: outside a git checkout (a packaged source tarball, a shallow
//! CI clone with no `.git`) this silently falls back to `"unknown"` — the
//! same "degrade loudly to a placeholder, never block the build" precedent
//! `local_rag_generate::LlamaGenerator::open` already sets for a missing
//! model file.

use std::path::Path;
use std::process::Command;

fn main() {
    watch_git_head();
    let build_id = git_describe().unwrap_or_else(|| "unknown".to_string());
    println!("cargo:rustc-env=LOCAL_RAG_BUILD_ID={build_id}");
}

/// Re-run this build script when the checked-out commit changes — `.git/HEAD`
/// itself (a branch switch) and the ref file it points at (an ordinary
/// commit on the current branch), since `HEAD` alone is a symbolic pointer
/// that does not change on every commit.
fn watch_git_head() {
    let git_dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../.git");
    let head_path = git_dir.join("HEAD");
    let Ok(head) = std::fs::read_to_string(&head_path) else {
        return;
    };
    println!("cargo:rerun-if-changed={}", head_path.display());
    if let Some(ref_path) = head.trim().strip_prefix("ref: ") {
        println!(
            "cargo:rerun-if-changed={}",
            git_dir.join(ref_path).display()
        );
    }
}

fn git_describe() -> Option<String> {
    let output = Command::new("git")
        .args(["describe", "--always", "--dirty"])
        .current_dir(env!("CARGO_MANIFEST_DIR"))
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let text = String::from_utf8(output.stdout).ok()?;
    let text = text.trim();
    if text.is_empty() {
        None
    } else {
        Some(text.to_string())
    }
}
