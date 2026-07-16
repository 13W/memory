//! Run a child process, capture its output, and preserve a debuggable artifact
//! bundle when it exits abnormally.
//!
//! Crash-matrix tests (F1–F12, S1–S8) spawn a subprocess and kill it at a named
//! point. When that child dies with a non-zero code or a signal, the raw
//! stdout/stderr are exactly the evidence a human needs. [`run_capturing`]
//! writes them to a bundle directory that lives *outside* any [`TempHome`], so
//! it survives the test that produced it.
//!
//! [`TempHome`]: crate::home::TempHome

use std::fs;
use std::io::{self, Write};
use std::path::PathBuf;
use std::process::{Command, ExitStatus};

use crate::next_seq;

/// The result of running a child process to completion.
#[derive(Debug)]
pub struct RunOutcome {
    /// The child's exit status.
    pub status: ExitStatus,
    /// Captured standard output.
    pub stdout: Vec<u8>,
    /// Captured standard error.
    pub stderr: Vec<u8>,
    /// Path to the persisted artifact bundle, present iff the child exited
    /// abnormally (`!status.success()`).
    pub bundle: Option<PathBuf>,
}

impl RunOutcome {
    /// Whether the child exited successfully (code 0, not signalled).
    pub fn success(&self) -> bool {
        self.status.success()
    }

    /// Captured stdout decoded lossily as UTF-8.
    pub fn stdout_lossy(&self) -> std::borrow::Cow<'_, str> {
        String::from_utf8_lossy(&self.stdout)
    }

    /// Captured stderr decoded lossily as UTF-8.
    pub fn stderr_lossy(&self) -> std::borrow::Cow<'_, str> {
        String::from_utf8_lossy(&self.stderr)
    }
}

/// Run `cmd` to completion, capturing its output.
///
/// On abnormal exit an artifact bundle is written under
/// `<temp>/local-rag-test-artifacts/<label>-<pid>-<seq>/` containing
/// `command.txt`, `stdout.log`, `stderr.log`, and `status.txt`, and its path is
/// returned in [`RunOutcome::bundle`]. On success no bundle is written.
pub fn run_capturing(mut cmd: Command, label: &str) -> io::Result<RunOutcome> {
    let command_line = render_command(&cmd);
    let output = cmd.output()?;

    let bundle = if output.status.success() {
        None
    } else {
        Some(write_bundle(
            label,
            &command_line,
            &output.stdout,
            &output.stderr,
            &output.status,
        )?)
    };

    Ok(RunOutcome {
        status: output.status,
        stdout: output.stdout,
        stderr: output.stderr,
        bundle,
    })
}

/// Render `program` plus its arguments as a single, lossy, human-readable line.
fn render_command(cmd: &Command) -> String {
    let mut parts = vec![cmd.get_program().to_string_lossy().into_owned()];
    parts.extend(cmd.get_args().map(|a| a.to_string_lossy().into_owned()));
    parts.join(" ")
}

/// Persist a crash bundle and return its directory.
fn write_bundle(
    label: &str,
    command_line: &str,
    stdout: &[u8],
    stderr: &[u8],
    status: &ExitStatus,
) -> io::Result<PathBuf> {
    let root = std::env::temp_dir().join("local-rag-test-artifacts");
    let dir = root.join(format!("{label}-{}-{}", std::process::id(), next_seq()));
    fs::create_dir_all(&dir)?;

    write_file(&dir.join("command.txt"), command_line.as_bytes())?;
    write_file(&dir.join("stdout.log"), stdout)?;
    write_file(&dir.join("stderr.log"), stderr)?;
    write_file(&dir.join("status.txt"), format!("{status}\n").as_bytes())?;

    Ok(dir)
}

fn write_file(path: &std::path::Path, bytes: &[u8]) -> io::Result<()> {
    let mut f = fs::File::create(path)?;
    f.write_all(bytes)
}
