//! Where asset bytes come from — the installer's one network seam.
//!
//! [`AssetFetcher`] exists so the installer's *policy* (atomic writes, digests,
//! resumability, the `.ok` marker) can be tested exhaustively without a network,
//! the same way `Sleeper`, `Clock`, `Env` and `UuidSource` isolate their own
//! side effects elsewhere in this workspace. Production uses [`HttpFetcher`];
//! tests use a local `TcpListener` (real HTTP against `127.0.0.1`, so the client
//! itself is covered) or [`LocalFetcher`] for cases where the transport is not
//! the subject.
//!
//! # Why downloading is not gated on `data_policy`
//!
//! Spec 12 §1's `local_only` and spec 10 §1's "every remote call is gated …
//! before the provider is selected" both govern *provider selection* — sending
//! repository content to an embedding backend. Fetching weights is the opposite
//! direction: an explicit user command pulls public bytes in, and nothing about
//! the user's code leaves the machine. Gating it on the default policy would
//! make a `local_only` install unable to obtain the local model at all, which
//! inverts the policy's intent. Recorded as a `[SPEC]` amendment in 12 §1 rather
//! than left implicit.

use std::fmt;
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};

/// How many bytes to move per read/write turn while streaming an asset.
///
/// Large enough to keep syscall overhead irrelevant next to a 300 MB download,
/// small enough that the installer never holds a meaningful buffer in memory.
const COPY_CHUNK: usize = 64 * 1024;

/// A failure while fetching bytes.
#[derive(Debug)]
#[non_exhaustive]
pub enum FetchError {
    /// The transport failed (connection, TLS, DNS, or a local read).
    Transport {
        /// The URL that failed.
        url: String,
        /// What went wrong.
        message: String,
    },
    /// The server answered, but not with a success status.
    Status {
        /// The URL that failed.
        url: String,
        /// The HTTP status code returned.
        status: u16,
    },
    /// Writing the fetched bytes to disk failed.
    Io(io::Error),
}

impl fmt::Display for FetchError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            FetchError::Transport { url, message } => {
                write!(f, "could not fetch {url}: {message}")
            }
            FetchError::Status { url, status } => {
                write!(f, "fetching {url} returned HTTP {status}")
            }
            FetchError::Io(e) => write!(f, "writing fetched bytes failed: {e}"),
        }
    }
}

impl std::error::Error for FetchError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            FetchError::Io(e) => Some(e),
            _ => None,
        }
    }
}

impl From<io::Error> for FetchError {
    fn from(e: io::Error) -> Self {
        FetchError::Io(e)
    }
}

/// Streams one asset's bytes into a writer.
///
/// Streaming rather than returning a `Vec<u8>` is deliberate: the default
/// model's largest file is ~295 MB, and the installer hashes as it writes, so
/// nothing ever needs the whole asset in memory.
pub trait AssetFetcher: Send + Sync {
    /// Copy everything at `url` into `sink`, returning the byte count.
    fn fetch(&self, url: &str, sink: &mut dyn Write) -> Result<u64, FetchError>;
}

/// The production fetcher: HTTPS via `ureq` (rustls).
#[derive(Debug, Default)]
pub struct HttpFetcher {
    agent: Option<ureq::Agent>,
}

impl HttpFetcher {
    /// A fetcher with `ureq`'s default agent configuration.
    pub fn new() -> Self {
        Self::default()
    }

    fn agent(&self) -> ureq::Agent {
        self.agent
            .clone()
            .unwrap_or_else(ureq::Agent::new_with_defaults)
    }
}

impl AssetFetcher for HttpFetcher {
    fn fetch(&self, url: &str, sink: &mut dyn Write) -> Result<u64, FetchError> {
        let response = self.agent().get(url).call().map_err(|e| match &e {
            ureq::Error::StatusCode(status) => FetchError::Status {
                url: url.to_string(),
                status: *status,
            },
            other => FetchError::Transport {
                url: url.to_string(),
                message: other.to_string(),
            },
        })?;
        let mut reader = response.into_body().into_reader();
        copy_stream(&mut reader, sink)
    }
}

/// A fetcher that serves assets from a local directory, addressing them by the
/// last path segment of the URL.
///
/// Two uses: a test that is not about the transport, and an operator installing
/// from a mirror they already have on disk.
#[derive(Debug, Clone)]
pub struct LocalFetcher {
    root: PathBuf,
}

impl LocalFetcher {
    /// Serve assets out of `root`.
    pub fn new(root: impl Into<PathBuf>) -> Self {
        LocalFetcher { root: root.into() }
    }

    fn resolve(&self, url: &str) -> PathBuf {
        let name = url.rsplit('/').next().unwrap_or(url);
        self.root.join(name)
    }
}

impl AssetFetcher for LocalFetcher {
    fn fetch(&self, url: &str, sink: &mut dyn Write) -> Result<u64, FetchError> {
        let path: &Path = &self.resolve(url);
        let mut file = std::fs::File::open(path).map_err(|e| FetchError::Transport {
            url: url.to_string(),
            message: format!("{}: {e}", path.display()),
        })?;
        copy_stream(&mut file, sink)
    }
}

/// Copy `reader` into `sink` in bounded chunks, returning the byte count.
fn copy_stream(reader: &mut dyn Read, sink: &mut dyn Write) -> Result<u64, FetchError> {
    let mut buf = vec![0u8; COPY_CHUNK];
    let mut total = 0u64;
    loop {
        let n = reader.read(&mut buf)?;
        if n == 0 {
            return Ok(total);
        }
        sink.write_all(&buf[..n])?;
        total += n as u64;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use local_rag_test_support::home::TempHome;

    #[test]
    fn a_local_fetcher_streams_a_file_by_url_tail() {
        let home = TempHome::new().expect("temp home");
        let dir = home.join("mirror");
        std::fs::create_dir_all(&dir).expect("temp dir");
        std::fs::write(dir.join("asset.bin"), b"hello weights").expect("seed");

        let fetcher = LocalFetcher::new(&dir);
        let mut sink = Vec::new();
        let n = fetcher
            .fetch("https://example.invalid/repo/asset.bin", &mut sink)
            .expect("fetch");

        assert_eq!(n, 13);
        assert_eq!(sink, b"hello weights");
    }

    #[test]
    fn a_missing_local_asset_is_a_transport_error() {
        let home = TempHome::new().expect("temp home");
        let fetcher = LocalFetcher::new(home.join("absent"));
        let mut sink = Vec::new();
        let err = fetcher
            .fetch("https://example.invalid/nope.bin", &mut sink)
            .expect_err("absent");
        assert!(matches!(err, FetchError::Transport { .. }), "{err}");
        assert!(sink.is_empty(), "nothing is written on failure");
    }

    #[test]
    fn copy_stream_moves_more_than_one_chunk() {
        let payload = vec![7u8; COPY_CHUNK * 2 + 13];
        let mut sink = Vec::new();
        let n = copy_stream(&mut payload.as_slice(), &mut sink).expect("copy");
        assert_eq!(n as usize, payload.len());
        assert_eq!(sink, payload);
    }
}
