//! `local-rag repo list` / `local-rag repo attach` (spec 11 §6, spec 04 §7).
//!
//! `--worktree` is an as-built `[SPEC]` refinement of the one-line spec
//! sketch (`repo attach <repo_id> [--path P]`, itself marked "[SPEC surface,
//! commands implied by design]"): `local_rag_store::registry::resolve`'s own
//! doc names the exact scenario it exists for — "two detached linked
//! worktrees of one repository are `Ambiguous`, since a repo-level hint
//! cannot choose between them" (spec 04 §7's "an explicit attach is
//! required"). Without `--worktree`, that case would have no CLI answer at
//! all.

use std::path::PathBuf;
use std::process::ExitCode;

use local_rag::daemon::gitroot;
use local_rag_store::{AttachError, Candidate, RequestRoot, Resolution, attach, resolve};

use super::index::open_state;
use super::{EXIT_USAGE, block_on, fail, resolve_layout_and_config, system_now_ms};

const BIN: &str = "local-rag";

pub fn run(mut args: impl Iterator<Item = String>) -> ExitCode {
    match args.next().as_deref() {
        Some("list") => run_list(args),
        Some("attach") => run_attach(args),
        Some(other) => {
            eprintln!("{BIN} repo: unknown subcommand {other:?}");
            ExitCode::from(EXIT_USAGE)
        }
        None => {
            eprintln!(
                "{BIN} repo: usage: {BIN} repo list|attach <repo_id> [--path P] [--worktree <id>]"
            );
            ExitCode::from(EXIT_USAGE)
        }
    }
}

fn run_list(args: impl Iterator<Item = String>) -> ExitCode {
    if let Some(extra) = args.into_iter().next() {
        eprintln!("{BIN} repo list: unknown argument {extra:?}");
        return ExitCode::from(EXIT_USAGE);
    }

    let (layout, _config) = match resolve_layout_and_config() {
        Ok(v) => v,
        Err(e) => return fail(BIN, &e),
    };
    let state = match open_state(&layout) {
        Ok(s) => s,
        Err(e) => return fail(BIN, &e),
    };
    let conn = match state.open_read() {
        Ok(c) => c,
        Err(e) => return fail(BIN, &format!("could not open state.sqlite: {e}")),
    };

    let ids = match local_rag_store::all_repository_ids(&conn) {
        Ok(ids) => ids,
        Err(e) => return fail(BIN, &format!("could not list repositories: {e}")),
    };
    if ids.is_empty() {
        println!("{BIN}: no repositories registered yet");
        return ExitCode::SUCCESS;
    }
    for repo_id in ids {
        let path = match local_rag_store::current_path(&conn, &repo_id) {
            Ok(Some(p)) => p,
            Ok(None) => "(no current path)".to_string(),
            Err(e) => {
                return fail(
                    BIN,
                    &format!("could not read {repo_id}'s current path: {e}"),
                );
            }
        };
        let count = match local_rag_store::worktrees_of_repo(&conn, &repo_id) {
            Ok(w) => w.len(),
            Err(e) => return fail(BIN, &format!("could not list {repo_id}'s worktrees: {e}")),
        };
        println!("{repo_id}  {path}  ({count} worktree(s))");
    }
    ExitCode::SUCCESS
}

fn run_attach(mut args: impl Iterator<Item = String>) -> ExitCode {
    let Some(repo_id) = args.next() else {
        eprintln!(
            "{BIN} repo attach: usage: {BIN} repo attach <repo_id> [--path P] [--worktree <id>]"
        );
        return ExitCode::from(EXIT_USAGE);
    };
    let mut path: Option<PathBuf> = None;
    let mut worktree_id: Option<String> = None;
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--path" => match args.next() {
                Some(v) => path = Some(PathBuf::from(v)),
                None => {
                    eprintln!("{BIN} repo attach: --path needs a value");
                    return ExitCode::from(EXIT_USAGE);
                }
            },
            "--worktree" => match args.next() {
                Some(v) => worktree_id = Some(v),
                None => {
                    eprintln!("{BIN} repo attach: --worktree needs a value");
                    return ExitCode::from(EXIT_USAGE);
                }
            },
            other => {
                eprintln!("{BIN} repo attach: unknown argument {other:?}");
                return ExitCode::from(EXIT_USAGE);
            }
        }
    }

    let (layout, _config) = match resolve_layout_and_config() {
        Ok(v) => v,
        Err(e) => return fail(BIN, &e),
    };
    let target = match path {
        Some(p) => p,
        None => match std::env::current_dir() {
            Ok(cwd) => cwd,
            Err(e) => {
                return fail(
                    BIN,
                    &format!("could not determine the current directory: {e}"),
                );
            }
        },
    };
    let Some(facts) = gitroot::probe(&target) else {
        return fail(
            BIN,
            &format!("{}: not an accessible directory", target.display()),
        );
    };
    let state = match open_state(&layout) {
        Ok(s) => s,
        Err(e) => return fail(BIN, &e),
    };

    let resolved_worktree_id = match worktree_id {
        // `--worktree` names the exact identity to bind; no resolution needed.
        Some(id) => id,
        None => {
            let conn = match state.open_read() {
                Ok(c) => c,
                Err(e) => return fail(BIN, &format!("could not open state.sqlite: {e}")),
            };
            let resolution = match resolve(
                &conn,
                &RequestRoot {
                    worktree_root: Some(facts.clone()),
                    repo_hint: Some(repo_id.clone()),
                },
            ) {
                Ok(r) => r,
                Err(e) => return fail(BIN, &format!("could not resolve worktree identity: {e}")),
            };
            drop(conn);
            match resolution {
                Resolution::Resolved {
                    repo_id: resolved_repo,
                    worktree_id,
                } if resolved_repo == repo_id => worktree_id,
                Resolution::Resolved { repo_id: other, .. } => {
                    return fail(
                        BIN,
                        &format!(
                            "{}: already resolves to repository {other}, not {repo_id}",
                            target.display()
                        ),
                    );
                }
                Resolution::Ambiguous { candidates } => {
                    let matching: Vec<Candidate> = candidates
                        .into_iter()
                        .filter(|c| c.repo_id == repo_id)
                        .collect();
                    if matching.is_empty() {
                        return fail(
                            BIN,
                            &format!(
                                "{}: no detached worktree of repository {repo_id} matches this path; \
                                 run `local-rag index {}` to index it as a new worktree",
                                target.display(),
                                target.display()
                            ),
                        );
                    }
                    eprintln!(
                        "{BIN}: {} matches {} detached worktree(s) of repository {repo_id}; \
                         pick one with --worktree:",
                        target.display(),
                        matching.len()
                    );
                    for c in &matching {
                        eprintln!("  worktree {} ({})", c.worktree_id, c.kind.as_str());
                    }
                    return ExitCode::FAILURE;
                }
                Resolution::GlobalOnly => {
                    return fail(
                        BIN,
                        &format!(
                            "{}: does not match any known worktree; run `local-rag index {}` \
                             to index it as a new worktree",
                            target.display(),
                            target.display()
                        ),
                    );
                }
            }
        }
    };

    let now_ms = system_now_ms();
    let outcome = block_on({
        let (repo_id, worktree_id, facts) = (repo_id.clone(), resolved_worktree_id.clone(), facts);
        async move {
            state
                .writer()
                .transaction(move |tx| attach(tx, &repo_id, &worktree_id, &facts, now_ms))
                .await
        }
    });

    match outcome {
        Ok(Ok(())) => {
            println!(
                "{BIN}: attached worktree {resolved_worktree_id} to repository {repo_id} at {}",
                target.display()
            );
            ExitCode::SUCCESS
        }
        Ok(Err(AttachError::UnknownWorktree)) => fail(
            BIN,
            &format!("worktree {resolved_worktree_id} does not exist"),
        ),
        Ok(Err(e @ AttachError::RepoMismatch { .. })) => fail(BIN, &e.to_string()),
        Ok(Err(e @ AttachError::NotReattachable(_))) => fail(BIN, &e.to_string()),
        Err(e) => fail(BIN, &format!("could not attach: {e}")),
    }
}
