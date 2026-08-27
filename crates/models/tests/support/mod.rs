//! Shared fixtures for the installer tests: a loopback HTTP server and a
//! miniature catalog entry.
//!
//! The card is explicit that "network tests use local fixture server only", so
//! every byte in these tests comes from `127.0.0.1` — a real socket, a real HTTP
//! response, and the production `HttpFetcher` on the client side. What is *not*
//! real is the model: the default entry is 295 MB, so the fixtures below build a
//! catalog entry out of a few hundred bytes whose digests are computed from the
//! same `sha256_hex` the installer verifies with.

#![allow(dead_code)]

use std::collections::HashMap;
use std::io::{BufRead, BufReader, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;

use local_rag_models::archive::ArchiveFormat;
use local_rag_models::ort_catalog::OrtAsset;
use local_rag_models::{AssetFetcher, AssetFile, FetchError, ModelCatalogEntry};
use local_rag_test_support::TempHome;

/// The fixture model's id — deliberately not the real one, so a test can never
/// be confused with an installation of the shipped default.
pub const FIXTURE_MODEL_ID: &str = "fixture-model";

/// The fixture files' contents, in install order.
pub const FIXTURE_FILES: &[(&str, &str, &[u8])] = &[
    (
        "weights.onnx",
        "onnx/weights.onnx",
        b"fixture weights, byte for byte",
    ),
    (
        "weights.onnx_data",
        "onnx/weights.onnx_data",
        b"external tensor data",
    ),
    ("tokenizer.json", "tokenizer.json", b"{\"fixture\": true}"),
];

/// Leak a `String` into the `&'static str` the catalog's data model uses.
///
/// The catalog is compiled-in production data, so its fields are `&'static`;
/// tests need runtime values (a digest, a port). Leaking a handful of small
/// strings per test process is the cheapest way to bridge that without making
/// the production type generic over its lifetime.
pub fn leak(s: String) -> &'static str {
    Box::leak(s.into_boxed_str())
}

/// Build a catalog entry for the fixture files served by `source`.
///
/// Digests are computed here rather than hardcoded: the fixture bytes and the
/// pinned `sha256` must agree by construction, or the tests would be asserting
/// against a typo instead of against the installer.
pub fn fixture_entry(source: &str) -> ModelCatalogEntry {
    let files: Vec<AssetFile> = FIXTURE_FILES
        .iter()
        .map(|(relative, source_path, bytes)| AssetFile {
            relative_path: relative,
            source_path,
            size: bytes.len() as u64,
            sha256: leak(local_rag_core::hash::sha256_hex(bytes)),
        })
        .collect();

    ModelCatalogEntry {
        model_id: FIXTURE_MODEL_ID,
        source: leak(source.to_string()),
        revision: "rev-0",
        license: "Fixture Terms of Use",
        license_url: "https://example.invalid/terms",
        dimensions: 8,
        files: Box::leak(files.into_boxed_slice()),
    }
}

/// The fixture bytes for `relative_path`.
pub fn fixture_bytes(relative_path: &str) -> &'static [u8] {
    FIXTURE_FILES
        .iter()
        .find(|(relative, _, _)| *relative == relative_path)
        .map(|(_, _, bytes)| *bytes)
        .expect("unknown fixture file")
}

/// A minimal HTTP server on `127.0.0.1:0` serving fixture assets.
///
/// It records every request path, so a test can assert on *how many* fetches an
/// install performed — that is what makes "resumed rather than restarted" and
/// "reused rather than re-downloaded" observable rather than inferred.
pub struct FixtureServer {
    addr: SocketAddr,
    bodies: Arc<Mutex<HashMap<String, Vec<u8>>>>,
    requests: Arc<Mutex<Vec<String>>>,
    shutdown: Arc<AtomicBool>,
    handle: Option<JoinHandle<()>>,
}

impl FixtureServer {
    /// Start a server that serves the fixture files under `/repo/resolve/rev-0/`.
    pub fn start() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind loopback");
        let addr = listener.local_addr().expect("local addr");

        let mut initial = HashMap::new();
        for (relative, source_path, bytes) in FIXTURE_FILES {
            let _ = relative;
            initial.insert(format!("/repo/resolve/rev-0/{source_path}"), bytes.to_vec());
        }

        let bodies = Arc::new(Mutex::new(initial));
        let requests = Arc::new(Mutex::new(Vec::new()));
        let shutdown = Arc::new(AtomicBool::new(false));

        let handle = {
            let bodies = Arc::clone(&bodies);
            let requests = Arc::clone(&requests);
            let shutdown = Arc::clone(&shutdown);
            std::thread::spawn(move || {
                for stream in listener.incoming() {
                    if shutdown.load(Ordering::SeqCst) {
                        return;
                    }
                    let Ok(stream) = stream else { return };
                    serve_one(stream, &bodies, &requests);
                }
            })
        };

        FixtureServer {
            addr,
            bodies,
            requests,
            shutdown,
            handle: Some(handle),
        }
    }

    /// The `source` URL a catalog entry should use to reach this server.
    pub fn source_url(&self) -> String {
        format!("http://{}/repo", self.addr)
    }

    /// A catalog entry pointing at this server.
    pub fn entry(&self) -> ModelCatalogEntry {
        fixture_entry(&self.source_url())
    }

    /// Replace what the server returns for `relative_path` — used to simulate a
    /// mirror that serves the right name with the wrong bytes.
    pub fn corrupt(&self, relative_path: &str, body: &[u8]) {
        let source_path = FIXTURE_FILES
            .iter()
            .find(|(relative, _, _)| *relative == relative_path)
            .map(|(_, source_path, _)| *source_path)
            .expect("unknown fixture file");
        self.bodies
            .lock()
            .expect("bodies mutex")
            .insert(format!("/repo/resolve/rev-0/{source_path}"), body.to_vec());
    }

    /// Every path requested so far, in order.
    pub fn requests(&self) -> Vec<String> {
        self.requests.lock().expect("requests mutex").clone()
    }

    /// How many requests have been served.
    pub fn request_count(&self) -> usize {
        self.requests.lock().expect("requests mutex").len()
    }
}

impl Drop for FixtureServer {
    fn drop(&mut self) {
        self.shutdown.store(true, Ordering::SeqCst);
        // Wake the blocking `accept` so the thread observes the flag.
        let _ = TcpStream::connect(self.addr);
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}

/// Handle exactly one request/response exchange.
fn serve_one(
    mut stream: TcpStream,
    bodies: &Arc<Mutex<HashMap<String, Vec<u8>>>>,
    requests: &Arc<Mutex<Vec<String>>>,
) {
    let mut reader = BufReader::new(match stream.try_clone() {
        Ok(clone) => clone,
        Err(_) => return,
    });

    let mut request_line = String::new();
    if reader.read_line(&mut request_line).is_err() || request_line.is_empty() {
        return;
    }
    // Drain headers; the fixture never reads a body.
    loop {
        let mut line = String::new();
        match reader.read_line(&mut line) {
            Ok(0) => break,
            Ok(_) if line == "\r\n" || line == "\n" => break,
            Ok(_) => {}
            Err(_) => return,
        }
    }

    let path = request_line
        .split_whitespace()
        .nth(1)
        .unwrap_or("/")
        .to_string();
    requests.lock().expect("requests mutex").push(path.clone());

    let body = bodies.lock().expect("bodies mutex").get(&path).cloned();
    let response = match body {
        Some(bytes) => {
            let mut head = format!(
                "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nContent-Type: application/octet-stream\r\nConnection: close\r\n\r\n",
                bytes.len()
            )
            .into_bytes();
            head.extend_from_slice(&bytes);
            head
        }
        None => {
            b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n".to_vec()
        }
    };
    let _ = stream.write_all(&response);
    let _ = stream.flush();
}

/// A fetcher that fails the test if it is ever called.
///
/// "Offline afterwards" (spec 10 §5) is a claim about what the code does *not*
/// do, and the only honest way to assert it is to make doing it fatal.
#[derive(Debug, Default)]
pub struct ForbiddenFetcher;

impl AssetFetcher for ForbiddenFetcher {
    fn fetch(&self, url: &str, _sink: &mut dyn Write) -> Result<u64, FetchError> {
        panic!("the installer reached the network for {url} when it must not have");
    }
}

/// A fetcher that records the model directory's contents before serving each
/// file, so a test can assert on install *ordering* rather than only on the end
/// state.
pub struct ObservingFetcher<F> {
    inner: F,
    watch: std::path::PathBuf,
    seen: Mutex<Vec<Vec<String>>>,
}

impl<F: AssetFetcher> ObservingFetcher<F> {
    /// Wrap `inner`, snapshotting `watch`'s entries on every fetch.
    pub fn new(inner: F, watch: impl Into<std::path::PathBuf>) -> Self {
        ObservingFetcher {
            inner,
            watch: watch.into(),
            seen: Mutex::new(Vec::new()),
        }
    }

    /// One sorted directory listing per fetch, in order.
    pub fn snapshots(&self) -> Vec<Vec<String>> {
        self.seen.lock().expect("snapshots mutex").clone()
    }
}

impl<F: AssetFetcher> AssetFetcher for ObservingFetcher<F> {
    fn fetch(&self, url: &str, sink: &mut dyn Write) -> Result<u64, FetchError> {
        let mut entries: Vec<String> = std::fs::read_dir(&self.watch)
            .map(|dir| {
                dir.filter_map(Result::ok)
                    .map(|e| e.file_name().to_string_lossy().into_owned())
                    .collect()
            })
            .unwrap_or_default();
        entries.sort();
        self.seen.lock().expect("snapshots mutex").push(entries);
        self.inner.fetch(url, sink)
    }
}

/// A tar.gz holding one member, plus a decoy, built in memory.
fn ort_fixture_archive(payload: &[u8]) -> Vec<u8> {
    use std::io::Write;
    fn header(name: &str, len: usize) -> [u8; 512] {
        let mut h = [0u8; 512];
        h[..name.len()].copy_from_slice(name.as_bytes());
        h[100..107].copy_from_slice(b"0000644");
        h[108..115].copy_from_slice(b"0000000");
        h[116..123].copy_from_slice(b"0000000");
        let size = format!("{len:011o} ");
        h[124..124 + size.len()].copy_from_slice(size.as_bytes());
        h[136..147].copy_from_slice(b"00000000000");
        h[156] = b'0';
        h[257..263].copy_from_slice(b"ustar\0");
        h[263..265].copy_from_slice(b"00");
        h[148..156].copy_from_slice(b"        ");
        let sum: u32 = h.iter().map(|&b| b as u32).sum();
        let chk = format!("{sum:06o}\0 ");
        h[148..148 + chk.len()].copy_from_slice(chk.as_bytes());
        h
    }
    let mut tar = Vec::new();
    for (name, data) in [
        ("ort/lib/decoy", b"nope".as_slice()),
        ("ort/lib/thing", payload),
    ] {
        tar.extend_from_slice(&header(name, data.len()));
        tar.extend_from_slice(data);
        tar.extend(std::iter::repeat_n(0u8, (512 - data.len() % 512) % 512));
    }
    tar.extend(std::iter::repeat_n(0u8, 1024));
    let mut e = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast());
    e.write_all(&tar).expect("gzip");
    e.finish().expect("finish gzip")
}

fn ort_sha256(bytes: &[u8]) -> String {
    local_rag_core::hash::sha256_hex(bytes)
}

pub struct OrtFixture {
    pub asset: OrtAsset,
    /// Directory a `LocalFetcher` serves the archive from.
    pub served: std::path::PathBuf,
}

pub fn ort_fixture(home: &TempHome, payload: &[u8]) -> OrtFixture {
    let archive = ort_fixture_archive(payload);
    let served = home.join("served");
    std::fs::create_dir_all(&served).expect("serve dir");
    std::fs::write(served.join("ort-fixture.tgz"), &archive).expect("write archive");
    OrtFixture {
        asset: OrtAsset {
            platform: "test-platform",
            version: "9.9.9",
            url: "https://example.invalid/ort-fixture.tgz",
            archive_format: ArchiveFormat::TarGz,
            archive_size: archive.len() as u64,
            archive_sha256: leak(ort_sha256(&archive)),
            archive_member: "ort/lib/thing",
            dylib_name: "libonnxruntime.test",
            dylib_size: payload.len() as u64,
            dylib_sha256: leak(ort_sha256(payload)),
        },
        served,
    }
}
